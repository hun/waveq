//! `run` / `eval`: execute a Lua script against the loaded dump.
//!
//! Unlike the declarative commands, both sides are first-class here: the same
//! `--b-file`/`--b-clock`/`--b-anchor`/`--b-cycle-offset` knobs that `compare`
//! uses are available to a script, and `--clock none` is allowed (scripts may
//! only want ticks).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result, bail};

use crate::cli::ScriptArgs;
use crate::clock;
use crate::load;
use crate::output::Doc;
use crate::resolve::ResolveCtx;
use crate::script::{LuaSide, ScriptCtx, ScriptOpts, ScriptOut};
use crate::sides::{self, Side};

use super::App;

/// `waveq run FILE|@NAME`.
pub fn run(app: &App, script: &str, opts: &ScriptArgs) -> Result<Doc> {
    let path = match script.strip_prefix('@') {
        Some(name) => {
            let Some(cfg) = app.cfg.script(name) else {
                let names = app.cfg.script_names();
                if names.is_empty() {
                    bail!("no script \"{script}\": the config defines no [scripts]");
                }
                bail!("no script \"{script}\"; scripts: {}", names.join(", "));
            };
            PathBuf::from(&cfg.file)
        }
        None => PathBuf::from(script),
    };
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read script {}", path.display()))?;
    execute(app, &source, &format!("@{}", path.display()), opts)
}

/// `waveq eval CODE`.
pub fn eval(app: &App, code: &str, opts: &ScriptArgs) -> Result<Doc> {
    execute(app, code, "=eval", opts)
}

fn execute(app: &App, source: &str, label: &str, opts: &ScriptArgs) -> Result<Doc> {
    let path_a = app.require_file()?.clone();
    let mut wf_a = load::full(&path_a, &load::load_options())?;
    let mut wf_b = match &app.b_file {
        Some(path) => Some(load::full(path, &load::load_options())?),
        None => None,
    };

    // A script may legitimately ask for ticks only, so both clocks are Option.
    let clock_a = clock::resolve_clock(&mut wf_a, app.clock_spec.as_deref(), app.edge, app.quiet)?;
    let clock_b = match wf_b.as_mut() {
        Some(wb) => clock::resolve_clock(wb, app.b_clock_spec.as_deref(), app.edge, app.quiet)?,
        None => match app.b_clock_spec.as_deref() {
            Some(spec) => clock::resolve_clock(&mut wf_a, Some(spec), app.edge, app.quiet)?,
            None => clock_a.clone(),
        },
    };

    let sign = app.sign_args(sides::hierarchy_of(&wf_a, wf_b.as_ref()).hierarchy())?;
    let rctx = ResolveCtx {
        types: &app.types,
        sign: &sign,
        radix: app.radix,
    };

    let anchor_a = sides::resolve_flag(
        &wf_a,
        wf_b.as_ref(),
        Side::A,
        &rctx,
        app.anchor.as_ref(),
        "--anchor",
    )?;
    let anchor_b = sides::resolve_flag(
        &wf_a,
        wf_b.as_ref(),
        Side::B,
        &rctx,
        app.b_anchor.as_ref(),
        "--b-anchor",
    )?;
    for handle in anchor_a.iter().chain(anchor_b.iter()) {
        match (handle.side, wf_b.as_mut()) {
            (Side::B, Some(wb)) => wb.load_signals_multi_threaded(&[handle.signal_ref]),
            _ => wf_a.load_signals_multi_threaded(&[handle.signal_ref]),
        }
    }

    let origin_a = match &clock_a {
        Some(info) => {
            sides::resolve_origin(Side::A, &wf_a, info, anchor_a.as_ref(), app.cycle_offset)?
        }
        None => 0,
    };
    let origin_b = match &clock_b {
        Some(info) => sides::side_b_origin(
            wf_b.is_none(),
            origin_a,
            sides::hierarchy_of(&wf_a, wf_b.as_ref()),
            info,
            anchor_b.as_ref(),
            app.b_cycle_offset,
        )?,
        None => 0,
    };

    let has_b = app.b_file.is_some()
        || app.b_clock_spec.is_some()
        || app.b_anchor.is_some()
        || app.b_cycle_offset != 0;

    let wf_a = Rc::new(RefCell::new(wf_a));
    let wf_b = wf_b.map(|wf| Rc::new(RefCell::new(wf)));
    let path_b = app.b_file.clone().unwrap_or_else(|| path_a.clone());
    let ctx = ScriptCtx {
        sides: [
            LuaSide {
                wf: wf_a.clone(),
                clock: clock_a,
                origin: origin_a,
                path: path_a,
            },
            LuaSide {
                wf: wf_b.unwrap_or_else(|| wf_a.clone()),
                clock: clock_b,
                origin: origin_b,
                path: path_b,
            },
        ],
        has_b,
        types: app.types.clone(),
        sign,
        radix: app.radix,
        format: app.format,
        quiet: app.quiet,
        out: RefCell::new(ScriptOut::default()),
    };
    let opts = ScriptOpts {
        budget: opts.budget,
        memory: opts.memory,
    };
    crate::script::run_chunk(Rc::new(ctx), source, label, &opts)
}
