//! Command dispatch and shared command context.

pub mod cmp;
pub mod cut;
pub mod introspect;
pub mod read;
pub mod script;

use anyhow::{Result, bail};
use std::path::PathBuf;
use wellen::{Hierarchy, Timescale};

use crate::cli::{Cli, Command, Global};
use crate::config::{Config, TypeTable};
use crate::format::{Edge, Radix, format_time};
use crate::output::{Doc, Format, print};
use crate::resolve::{ResolveCtx, SigHandle, SignArgs};
use crate::{config, load};

/// Everything a command needs to know about the invocation.
pub struct App {
    pub cli: Cli,
    pub cfg: Config,
    pub cfg_path: Option<PathBuf>,
    pub file: Option<PathBuf>,
    /// Second dump for side B (default: side B lives in `file`).
    pub b_file: Option<PathBuf>,
    /// Default expected-value file for `@` B sides.
    pub expect_file: Option<PathBuf>,
    pub types: TypeTable,
    pub format: Format,
    pub edge: Edge,
    pub clock_spec: Option<String>,
    pub b_clock_spec: Option<String>,
    /// Skip the first N cycles of side B.
    pub b_cycle_offset: i64,
    /// Skip the first N cycles of side A.
    pub cycle_offset: i64,
    /// Logical cycle 0 of side A/B = first cycle where this signal is 1.
    pub anchor: Option<String>,
    pub b_anchor: Option<String>,
    pub radix: Option<Radix>,
    pub quiet: bool,
}

impl App {
    pub fn new(cli: Cli) -> Result<App> {
        let g: &Global = &cli.global;
        if g.config.is_some() && g.no_config {
            bail!("--config and --no-config are mutually exclusive");
        }
        let (cfg, cfg_path) = if g.no_config {
            (Config::default(), None)
        } else if let Some(path) = &g.config {
            (config::load(path)?, Some(path.clone()))
        } else {
            match std::env::current_dir()
                .ok()
                .and_then(|cwd| config::discover(&cwd))
            {
                Some(path) => (config::load(&path)?, Some(path)),
                None => (Config::default(), None),
            }
        };
        if let Some(path) = &cfg_path
            && !g.quiet
        {
            eprintln!("using config {}", path.display());
        }
        let file = load::resolve_path_opt(g.file.as_ref(), cfg.wave_file());
        let b_file = g
            .b_file
            .clone()
            .or_else(|| cfg.wave_b_file().map(PathBuf::from));
        let expect_file = g.expect_file.clone().or_else(|| {
            cfg.compare
                .as_ref()
                .and_then(|c| c.expect_file.clone())
                .map(PathBuf::from)
        });
        let edge = g.edge.or(cfg.wave_edge()).unwrap_or(Edge::Posedge);
        let clock_spec = g
            .clock
            .clone()
            .or_else(|| cfg.wave_clock().map(|s| s.to_string()));
        let b_clock_spec = g
            .b_clock
            .clone()
            .or_else(|| cfg.wave_b_clock().map(|s| s.to_string()));
        let b_cycle_offset = g.b_cycle_offset.or(cfg.wave_b_cycle_offset()).unwrap_or(0);
        let cycle_offset = g.cycle_offset.or(cfg.wave_cycle_offset()).unwrap_or(0);
        let anchor = g
            .anchor
            .clone()
            .or_else(|| cfg.wave_anchor().map(|s| s.to_string()));
        let b_anchor = g
            .b_anchor
            .clone()
            .or_else(|| cfg.wave_b_anchor().map(|s| s.to_string()));
        let types = TypeTable::from_config(&cfg);
        types.validate()?;
        Ok(App {
            format: g.format.unwrap_or(Format::Text),
            quiet: g.quiet,
            radix: g.radix,
            edge,
            clock_spec,
            b_clock_spec,
            b_cycle_offset,
            cycle_offset,
            anchor,
            b_anchor,
            b_file,
            expect_file,
            file,
            cfg,
            cfg_path,
            types,
            cli,
        })
    }

    pub fn global(&self) -> &Global {
        &self.cli.global
    }

    /// The wave file, or the documented "no wave file" error.
    pub fn require_file(&self) -> Result<&PathBuf> {
        self.file.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no wave file: pass -f <PATH> or set wave.file in .waveq.toml")
        })
    }

    pub fn note(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }

    /// Resolve CLI signedness arguments against the hierarchy.
    pub fn sign_args(&self, h: &Hierarchy) -> Result<SignArgs> {
        let g = self.global();
        SignArgs::resolve(h, g.signed, g.unsigned, &g.signed_sig, &g.unsigned_sig)
    }

    pub fn ctx<'a>(&'a self, sign: &'a SignArgs) -> ResolveCtx<'a> {
        ResolveCtx {
            types: &self.types,
            sign,
            radix: self.radix,
        }
    }

    /// `--max-rows` with a per-command default; `0` means unlimited.
    pub fn max_rows(&self, default: usize) -> Option<usize> {
        match self.global().max_rows.unwrap_or(default) {
            0 => None,
            n => Some(n),
        }
    }

    /// Expand `@group` references in `--signal`/`--probe` lists.
    pub fn expand_signals(&self, specs: &[String]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for spec in specs {
            if let Some(group) = spec.strip_prefix('@') {
                match self.cfg.signal_group(group) {
                    Some(members) => out.extend(members.iter().cloned()),
                    None => {
                        let names = self.cfg.group_names();
                        if names.is_empty() {
                            bail!(
                                "no signal group \"{group}\": the config defines no [signals] groups"
                            );
                        }
                        bail!("no signal group \"{group}\"; groups: {}", names.join(", "));
                    }
                }
            } else {
                out.push(spec.clone());
            }
        }
        if out.is_empty() {
            bail!(
                "no signals given: pass --signal <NAME> (repeatable, comma separated, @group allowed)"
            );
        }
        Ok(out)
    }

    /// Resolve a list of specs into handles (hierarchy already loaded by the caller).
    pub fn resolve_all(
        &self,
        h: &Hierarchy,
        sign: &SignArgs,
        specs: &[String],
    ) -> Result<Vec<SigHandle>> {
        let ctx = self.ctx(sign);
        specs
            .iter()
            .map(|s| crate::resolve::resolve_sig(h, &ctx, s))
            .collect()
    }
}

/// `--b-file`/`--b-clock`/`--b-anchor`/`--b-cycle-offset` only mean something for
/// `align`/`compare`, which read two sides.
pub fn guard_single_dump(app: &App) -> Result<()> {
    let g = app.global();
    if g.b_file.is_some()
        || g.b_clock.is_some()
        || g.b_anchor.is_some()
        || g.b_cycle_offset.unwrap_or(0) != 0
        || g.expect_file.is_some()
    {
        bail!(
            "--b-file/--b-clock/--b-anchor/--b-cycle-offset need two sides: they apply to \
             `align` and `compare` only"
        );
    }
    Ok(())
}

/// Resolve `--anchor` for the single-dump commands: the local edge index that
/// becomes logical cycle 0.
pub fn resolve_anchor(
    app: &App,
    wf: &mut wellen::simple::Waveform,
    clock: &crate::clock::ClockInfo,
) -> Result<i64> {
    guard_single_dump(app)?;
    let spec = match app.anchor.as_ref() {
        // --cycle-offset alone still shifts cycle numbering
        None => return Ok(app.cycle_offset),
        Some(spec) => spec,
    };
    let anchor = {
        let sign = app.sign_args(wf.hierarchy())?;
        let ctx = app.ctx(&sign);
        crate::resolve::resolve_sig(wf.hierarchy(), &ctx, spec)
            .map_err(|e| anyhow::anyhow!("--anchor: {e}"))?
    };
    anchor.load(wf);
    crate::sides::resolve_origin(
        crate::sides::Side::A,
        wf,
        clock,
        Some(&anchor),
        app.cycle_offset,
    )
}

pub fn ts_of(h: &Hierarchy) -> Option<Timescale> {
    h.timescale()
}

pub fn fmt_time(ts: Option<Timescale>, tick: u64) -> String {
    format_time(tick, ts)
}

pub fn cycle_str(cycle: i64) -> String {
    if cycle < 0 {
        "-".to_string()
    } else {
        cycle.to_string()
    }
}

pub fn cycle_json(cycle: i64) -> serde_json::Value {
    if cycle < 0 {
        serde_json::Value::Null
    } else {
        cycle.into()
    }
}

/// Parse the repeated `--match`/`--scope` filters into a predicate.
pub fn keep_signal(matches: &[String], prefix: Option<&str>, full_name: &str) -> Result<bool> {
    if let Some(prefix) = prefix
        && !full_name.starts_with(prefix)
    {
        return Ok(false);
    }
    if matches.is_empty() {
        return Ok(true);
    }
    crate::resolve::matches_any(matches, full_name)
}

pub fn dispatch(cli: Cli) -> Result<Doc> {
    let app = App::new(cli)?;
    let doc = match &app.cli.command {
        Command::Info { full, with_clock } => introspect::info(&app, *full, *with_clock)?,
        Command::Scopes { match_, depth } => introspect::scopes(&app, match_, *depth)?,
        Command::Signals {
            match_,
            scope,
            count,
            tree,
        } => introspect::signals(&app, match_, scope.as_deref(), *count, *tree)?,
        Command::Signal { name, bits } => introspect::signal(&app, name, bits.as_deref())?,
        Command::Clock { list } => introspect::clock(&app, *list)?,
        Command::Config => introspect::config(&app)?,
        Command::Value { at, cycle, signal } => read::value(&app, at.as_deref(), *cycle, signal)?,
        Command::Changes {
            signal,
            from_cycle,
            to_cycle,
            from,
            to,
            delta,
            no_initial,
            at_edges_only,
        } => read::changes(
            &app,
            signal,
            *from_cycle,
            *to_cycle,
            from.as_deref(),
            to.as_deref(),
            *delta,
            *no_initial,
            *at_edges_only,
        )?,
        Command::Cut {
            cycles,
            from_cycle,
            to_cycle,
            signal,
            output,
            all,
        } => cut::cut(
            &app,
            cut::CutArgs {
                cycles: cycles.clone(),
                from_cycle: *from_cycle,
                to_cycle: *to_cycle,
                signal: signal.clone(),
                output: output.clone(),
                all: *all,
            },
        )?,
        Command::Table {
            signal,
            cycles,
            from_cycle,
            to_cycle,
            at_cycle,
            context,
            time,
            query,
        } => read::table(
            &app,
            signal,
            cycles.as_deref(),
            *from_cycle,
            *to_cycle,
            *at_cycle,
            *context,
            time.as_deref(),
            query.as_deref(),
        )?,
        Command::Align {
            a,
            b,
            align,
            shifts,
            max_shift,
            max_delta,
            from_cycle,
            to_cycle,
            from,
            to,
            gate,
            in_valid,
            in_ready,
            out_valid,
            out_ready,
            cmp,
            x,
        } => cmp::align(
            &app,
            cmp::AlignArgs {
                a: a.clone(),
                b: b.clone(),
                shifts: shifts.clone(),
                common: cmp::CommonCli {
                    align: *align,
                    cmp: *cmp,
                    x: *x,
                    max_shift: *max_shift,
                    max_delta: *max_delta,
                    gate: gate.clone(),
                    in_valid: in_valid.clone(),
                    in_ready: in_ready.clone(),
                    out_valid: out_valid.clone(),
                    out_ready: out_ready.clone(),
                    from_cycle: *from_cycle,
                    to_cycle: *to_cycle,
                    from: from.clone(),
                    to: to.clone(),
                },
            },
        )?,
        Command::Compare {
            map,
            check,
            shift,
            pair_scopes,
            pair_by,
            map_glob,
            delta,
            max_delta,
            probe_at,
            auto_shift,
            align,
            gate,
            in_valid,
            in_ready,
            out_valid,
            out_ready,
            from_cycle,
            to_cycle,
            from,
            to,
            cmp,
            x,
            max_shift,
            probe,
            first_only,
            at_edges_only,
        } => cmp::compare(
            &app,
            cmp::CompareArgs {
                map: map.clone(),
                check: check.clone(),
                shift: *shift,
                auto_shift: *auto_shift,
                pair_scopes: pair_scopes.clone(),
                pair_by: pair_by.clone(),
                map_glob: map_glob.clone(),
                delta: *delta,
                probe_at: Some(*probe_at),
                probe: probe.clone(),
                first_only: *first_only,
                at_edges_only: *at_edges_only,
                common: cmp::CommonCli {
                    align: *align,
                    cmp: *cmp,
                    x: *x,
                    max_shift: *max_shift,
                    max_delta: *max_delta,
                    gate: gate.clone(),
                    in_valid: in_valid.clone(),
                    in_ready: in_ready.clone(),
                    out_valid: out_valid.clone(),
                    out_ready: out_ready.clone(),
                    from_cycle: *from_cycle,
                    to_cycle: *to_cycle,
                    from: from.clone(),
                    to: to.clone(),
                },
            },
        )?,
        Command::Run { script, opts } => crate::cmd::script::run(&app, script, opts)?,
        Command::Eval { code, opts } => crate::cmd::script::eval(&app, code, opts)?,
    };
    print(&doc, app.format);
    Ok(doc)
}
