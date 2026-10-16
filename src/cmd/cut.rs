//! `cut`: write a cycle/tick window of a dump as a small, self-contained VCD.
//!
//! The window is given in cycles (`--cycles A..B`, `--from-cycle`,
//! `--to-cycle`, resolved through the clock exactly like `table` does) or, when
//! no cycle bound is given, as the whole file. Bounds are never clamped: a
//! cycle outside the dump's cycle range is an error (exit 2).
//!
//! Signals come from `--signal` (repeatable, comma separated, `@group`
//! allowed) and may be globs (`tb_top.q*`, matched case-sensitively against
//! `full_name`); `--all` or no `--signal` at all means every signal in the
//! file. A `NAME[msb:lsb]` selection is rejected rather than silently widened
//! (see [`select_vars`]).
//!
//! The resolved clock is always included in the cut, even when the selection
//! does not mention it: without it the cut has no cycle frame at all (a cut of
//! `dut.*`/`ref.*` would auto-detect some *data* signal as the clock, or fail),
//! and its cycle boundaries must be the source's. Because a window only
//! contains the window, cycle `i` of a cut of source cycles `A..B` is source
//! cycle `A + i`; the summary carries a `read_with` recipe
//! (`--cycle-offset -A`) that restores the source numbering for `table`,
//! `changes` and `compare`. The clock addition is announced on stderr and in
//! the `added_clock` document field.
//!
//! Output routing:
//!
//! * `-o FILE` writes the VCD there and prints the summary document
//!   (`from_cycle`, `from_tick`, `changes`, …) on stdout;
//! * without `-o` the VCD *is* stdout — the summary is redirected to stderr
//!   (via [`App::note`](crate::cmd::App::note)) and the returned document is
//!   marked as suppressed so `waveq cut … > small.vcd` stays clean for every
//!   `--format`, including `json`/`jsonl`.

use std::path::PathBuf;

use anyhow::{Result, bail};
use wellen::{SignalRef, VarRef, simple::Waveform};

use crate::format::format_time;
use crate::output::{Doc, Field, Format};
use crate::resolve;
use crate::vcd::{self, CutComment};
use crate::{clock, load};

use super::App;

/// Arguments of the `cut` command (mirrors `cli::Command::Cut`).
#[derive(Debug, Clone, Default)]
pub struct CutArgs {
    /// Cycle range `A..B` (logical cycles of the resolved clock).
    pub cycles: Option<String>,
    /// First cycle of the window.
    pub from_cycle: Option<i64>,
    /// Last cycle of the window.
    pub to_cycle: Option<i64>,
    /// Signals to keep (specs, `@group`, or globs containing `*`/`?`).
    pub signal: Vec<String>,
    /// Write the VCD to this file instead of stdout.
    pub output: Option<PathBuf>,
    /// Keep every signal in the file.
    pub all: bool,
}

pub fn cut(app: &App, args: CutArgs) -> Result<Doc> {
    let path = app.require_file()?;
    // `cut` writes one sides' worth of window: the two-side flags and the
    // cycle-numbering flags would otherwise be silently ignored (a cycle window
    // is always taken in the file's own cycles).
    super::guard_single_dump(app)?;
    if app.anchor.is_some() || app.cycle_offset != 0 {
        bail!(
            "cut takes a cycle window in the file's own cycles: --anchor/--cycle-offset are \
             not supported here (cut a whole window and use --cycle-offset when reading it back)"
        );
    }
    let mut wf = load::full(path, &load::load_options())?;
    let ts = wf.hierarchy().timescale();

    let mut vars = select_vars(app, &wf, &args)?;
    if vars.is_empty() {
        bail!("empty selection: no signal to cut (pass --signal <NAME> or --all)");
    }

    let wants_cycles =
        args.cycles.is_some() || args.from_cycle.is_some() || args.to_cycle.is_some();
    let clock_info =
        match clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet) {
            Ok(info) => info,
            Err(err) => {
                // A whole-file cut does not need a clock; an explicit --clock NAME
                // that does not resolve is still the user's error.
                if wants_cycles || app.clock_spec.is_some() {
                    return Err(err);
                }
                app.note(&format!(
                    "cut: no clock resolved, cycle bounds unavailable ({err})"
                ));
                None
            }
        };

    // The window is cycle-addressed and a VCD has no other way to say "cycle 0
    // is this edge", so the resolved clock is always part of the cut, even when
    // the selection does not mention it.
    let mut added_clock = None;
    if let Some(info) = clock_info.as_ref()
        && !vars.contains(&info.var)
    {
        vars.push(info.var);
        added_clock = Some(info.full_name.clone());
    }
    if let Some(name) = added_clock.as_deref() {
        app.note(&format!(
            "cut: added the clock signal {name} to keep cycle numbering"
        ));
    }

    {
        let refs: Vec<SignalRef> = {
            let h = wf.hierarchy();
            vars.iter().map(|v| h[*v].signal_ref()).collect()
        };
        wf.load_signals_multi_threaded(&refs);
    }

    let (from_tick, to_tick, from_cycle, to_cycle) = if wants_cycles {
        let Some(info) = clock_info.as_ref() else {
            bail!("cycle bounds need a clock: do not pass --clock none here");
        };
        let (mut a, mut b) = match args.cycles.as_deref() {
            Some(range) => parse_cycle_range(range)?,
            None => (0, info.last_cycle()),
        };
        if let Some(from) = args.from_cycle {
            a = a.max(from);
        }
        if let Some(to) = args.to_cycle {
            b = b.min(to);
        }
        let first_edge = info.edges[0];
        if a < 0 {
            bail!(
                "cycle range starts before the first clock edge (first edge at {} (tick {first_edge}))",
                format_time(first_edge, ts)
            );
        }
        if a > info.last_cycle() {
            bail!(
                "cycle {a} is out of range for clock \"{}\" (0..{})",
                info.full_name,
                info.last_cycle()
            );
        }
        if b < a {
            bail!("empty cycle range {a}..{b}");
        }
        if b > info.last_cycle() {
            bail!(
                "cycle {b} is out of range for clock \"{}\" (0..{})",
                info.full_name,
                info.last_cycle()
            );
        }
        (
            info.edges[a as usize],
            info.edges[b as usize],
            Some(a),
            Some(b),
        )
    } else {
        let table = wf.time_table();
        if table.is_empty() {
            bail!("the file contains no value changes: nothing to cut");
        }
        let (first, last) = (table[0], table[table.len() - 1]);
        let non_neg = |cycle: i64| (cycle >= 0).then_some(cycle);
        match clock_info.as_ref() {
            Some(c) => (
                first,
                last,
                non_neg(c.cycle_of(first)),
                non_neg(c.cycle_of(last)),
            ),
            None => (first, last, None, None),
        }
    };

    let clock_name = clock_info.as_ref().map(|c| c.full_name.clone());
    let source = path.display().to_string();
    let stats = {
        let h = wf.hierarchy();
        vcd::write_vcd_with_comment(
            args.output.as_deref(),
            h,
            &wf,
            &vars,
            from_tick,
            to_tick,
            &CutComment {
                source: Some(&source),
                clock: clock_name.as_deref(),
                from_cycle,
                to_cycle,
            },
        )?
    };

    let output_text = match &args.output {
        Some(p) => p.display().to_string(),
        None => "-".to_string(),
    };
    // A window cut re-bases cycles on its own first edge; `--cycle-offset`
    // shifts the numbering back so the source's cycles can be used verbatim.
    let read_with = from_cycle
        .filter(|from| *from != 0)
        .map(|from| format!("waveq -f {output_text} --cycle-offset -{from} ..."));
    let mut doc = Doc::new()
        .field(Field::s("file", source))
        .field(Field::opt_s("clock", clock_name))
        .field(Field::opt_n("from_cycle", from_cycle))
        .field(Field::opt_n("to_cycle", to_cycle))
        .field(Field::u("from_tick", stats.from_tick))
        .field(Field::u("to_tick", stats.to_tick))
        .field(Field::s("from_time", format_time(stats.from_tick, ts)))
        .field(Field::s("to_time", format_time(stats.to_tick, ts)))
        .field(Field::u("signals", stats.signals as u64))
        .field(Field::u("changes", stats.changes as u64))
        .field(Field::s("output", output_text));
    if let Some(name) = added_clock {
        doc = doc.trailer_field(Field::s("added_clock", name));
    }
    // A cut re-bases cycles on its own first edge, so a windowed cut needs the
    // source cycle numbers restored explicitly when it is read back.
    if let Some(recipe) = read_with {
        doc = doc.trailer_field(Field::s("read_with", recipe.clone()));
        app.note(&recipe);
    }

    if args.output.is_some() {
        Ok(doc)
    } else {
        // stdout carries the VCD: the summary goes to stderr and the document
        // must not be printed next to the VCD bytes.
        app.note(doc.render(Format::Text).trim_end());
        Ok(doc.suppress_output(true))
    }
}

/// Resolve `--signal` into the variables to write, preserving the order the
/// user gave (globs expand in hierarchy order) and dropping duplicates.
///
/// A `NAME[msb:lsb]` selection is an error: a cut that silently contains all
/// bits of the signal would compare like the whole signal, which is exactly
/// the silent widening `cut` must not do.
fn select_vars(app: &App, wf: &Waveform, args: &CutArgs) -> Result<Vec<VarRef>> {
    let h = wf.hierarchy();
    if args.all || args.signal.is_empty() {
        return Ok(h.all_vars().collect());
    }
    let specs = app.expand_signals(&args.signal)?;
    let sign = app.sign_args(h)?;
    let ctx = app.ctx(&sign);
    let mut out: Vec<VarRef> = Vec::new();
    for spec in &specs {
        if spec.contains('*') || spec.contains('?') {
            let mut hits = 0usize;
            for var in h.all_vars() {
                if resolve::matches_glob(spec, &resolve::var_full_name(h, var))? {
                    hits += 1;
                    if !out.contains(&var) {
                        out.push(var);
                    }
                }
            }
            if hits == 0 {
                bail!(
                    "no signal matches \"{spec}\"; check it with `waveq signals --match '{spec}'`"
                );
            }
        } else {
            let parsed = resolve::parse_sigspec(spec)?;
            if let Some((msb, lsb)) = parsed.bits {
                bail!(
                    "bit-sliced cuts are not supported: \"{}[{msb}:{lsb}]\"; select the whole signal \"{}\" instead",
                    parsed.name,
                    parsed.name
                );
            }
            let handle = resolve::resolve_sig(h, &ctx, spec)?;
            if !out.contains(&handle.var) {
                out.push(handle.var);
            }
        }
    }
    Ok(out)
}

fn parse_cycle_range(s: &str) -> Result<(i64, i64)> {
    let (a, b) = s
        .split_once("..")
        .ok_or_else(|| anyhow::anyhow!("--cycles expects A..B, got \"{s}\""))?;
    let a: i64 = a
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("--cycles: invalid start in \"{s}\""))?;
    let b: i64 = b
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("--cycles: invalid end in \"{s}\""))?;
    Ok((a, b))
}
