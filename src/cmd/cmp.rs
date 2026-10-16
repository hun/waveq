//! `align` and `compare`: shift scanning, transfer indexing and divergence
//! classification — the semantic core of the golden-vs-pipelined workflow.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use wellen::Timescale;
use wellen::simple::Waveform;

use crate::clock::ClockInfo;
use crate::config::CheckCfg;
use crate::format::{AlignMode, CmpMode, XMode, format_time, parse_time};
use crate::output::{Doc, Field, Table};
use crate::resolve::SigHandle;
use crate::sample::{Cell, Val};
use crate::{clock, load, resolve};

use super::{App, fmt_time};

/// CLI inputs shared by `align` and `compare`.
#[derive(Debug, Clone, Default)]
pub struct CommonCli {
    pub align: Option<AlignMode>,
    pub cmp: Option<CmpMode>,
    pub x: Option<XMode>,
    pub max_shift: Option<u32>,
    pub gate: Option<String>,
    pub in_valid: Option<String>,
    pub in_ready: Option<String>,
    pub out_valid: Option<String>,
    pub out_ready: Option<String>,
    pub from_cycle: Option<i64>,
    pub to_cycle: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Fully resolved comparison options.
#[derive(Debug, Clone)]
pub struct Options {
    pub align: AlignMode,
    pub cmp: CmpMode,
    pub x: XMode,
    pub max_shift: u32,
    pub gate: Option<String>,
    pub in_valid: Option<String>,
    pub in_ready: Option<String>,
    pub out_valid: Option<String>,
    pub out_ready: Option<String>,
    pub from_cycle: Option<i64>,
    pub to_cycle: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
}

pub struct AlignArgs {
    pub a: String,
    pub b: String,
    pub shifts: Option<String>,
    pub common: CommonCli,
}

pub struct CompareArgs {
    pub map: Vec<String>,
    pub check: Option<String>,
    /// Forces the shift of every pair (CLI beats a named check's shift).
    pub shift: Option<u32>,
    pub auto_shift: bool,
    pub probe: Vec<String>,
    pub first_only: bool,
    pub at_edges_only: bool,
    pub common: CommonCli,
}

fn pick_s(cli: Option<&String>, named: Option<&String>, cfg: Option<&String>) -> Option<String> {
    cli.cloned()
        .or_else(|| named.cloned())
        .or_else(|| cfg.cloned())
}

fn pick<T: Copy>(cli: Option<T>, named: Option<T>, cfg: Option<T>) -> Option<T> {
    cli.or(named).or(cfg)
}

/// CLI > named check (`--check`) > `[compare]` section > built-in default.
pub fn resolve_options(app: &App, cli: &CommonCli, named: Option<&CheckCfg>) -> Options {
    let cfg = app.cfg.compare.clone().unwrap_or_default();
    let get = |f: fn(&CheckCfg) -> Option<&String>| -> Option<&String> { named.and_then(|c| f(c)) };
    Options {
        align: pick(cli.align, named.and_then(|c| c.align), cfg.align).unwrap_or(AlignMode::Cycles),
        cmp: pick(cli.cmp, named.and_then(|c| c.cmp), cfg.cmp).unwrap_or(CmpMode::BitExact),
        x: pick(cli.x, named.and_then(|c| c.x), cfg.x).unwrap_or(XMode::Mismatch),
        max_shift: pick(
            cli.max_shift,
            named.and_then(|c| c.max_shift),
            cfg.max_shift,
        )
        .unwrap_or(16),
        gate: pick_s(
            cli.gate.as_ref(),
            get(|c| c.gate.as_ref()),
            cfg.gate.as_ref(),
        ),
        in_valid: pick_s(
            cli.in_valid.as_ref(),
            get(|c| c.in_valid.as_ref()),
            cfg.in_valid.as_ref(),
        ),
        in_ready: pick_s(
            cli.in_ready.as_ref(),
            get(|c| c.in_ready.as_ref()),
            cfg.in_ready.as_ref(),
        ),
        out_valid: pick_s(
            cli.out_valid.as_ref(),
            get(|c| c.out_valid.as_ref()),
            cfg.out_valid.as_ref(),
        ),
        out_ready: pick_s(
            cli.out_ready.as_ref(),
            get(|c| c.out_ready.as_ref()),
            cfg.out_ready.as_ref(),
        ),
        from_cycle: cli.from_cycle.or(named.and_then(|c| c.from_cycle)),
        to_cycle: cli.to_cycle.or(named.and_then(|c| c.to_cycle)),
        from: pick_s(cli.from.as_ref(), get(|c| c.from.as_ref()), None),
        to: pick_s(cli.to.as_ref(), get(|c| c.to.as_ref()), None),
    }
}

// ---------------------------------------------------------------------------
// comparison kernel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Verdict {
    mismatch: bool,
    x_involved: bool,
}

impl Verdict {
    fn label(self) -> &'static str {
        if self.x_involved {
            "x-involved"
        } else if self.mismatch {
            "mismatch"
        } else {
            "ok"
        }
    }
}

/// One compared unit, in cycle or transfer space.
#[derive(Debug, Clone)]
struct Divergence {
    /// cycle (cycles mode) or transfer index (transfers mode)
    index: i64,
    a_cycle: i64,
    b_cycle: i64,
    verdict: Verdict,
    a_cell: Cell,
    b_cell: Cell,
}

#[derive(Debug, Clone, Default)]
struct Scan {
    compared: u64,
    mismatches: u64,
    x_involved: u64,
    skipped: u64,
    first_div: Option<Divergence>,
    divergences: Vec<Divergence>,
    a_events: usize,
    b_events: usize,
    paired: usize,
    unpaired: usize,
}

fn bits_to_i128(bits: &str, signed: bool, width: u32) -> i128 {
    let raw = crate::format::bits_to_u64(bits);
    if signed && width > 0 && width < 64 {
        let shift = 64 - width;
        i128::from(((raw << shift) as i64) >> shift)
    } else if signed && width >= 64 {
        i128::from(raw as i64)
    } else {
        i128::from(raw)
    }
}

fn numeric_eq(
    a: &Val,
    a_signed: bool,
    a_width: u32,
    b: &Val,
    b_signed: bool,
    b_width: u32,
) -> bool {
    match (a, b) {
        (Val::Bits(ab), Val::Bits(bb)) => {
            bits_to_i128(ab, a_signed, a_width) == bits_to_i128(bb, b_signed, b_width)
        }
        (Val::Real(x), Val::Real(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Event, Val::Event) => true,
        _ => false,
    }
}

fn classify(
    a: &Cell,
    a_handle: &SigHandle,
    b: &Cell,
    b_handle: &SigHandle,
    opts: &Options,
) -> Verdict {
    let (a_val, b_val) = match (&a.val, &b.val) {
        (Some(x), Some(y)) => (x, y),
        _ => {
            return Verdict {
                mismatch: false,
                x_involved: false,
            };
        }
    };
    let x_involved = a_val.has_xz() || b_val.has_xz();
    if x_involved {
        return Verdict {
            mismatch: opts.x == XMode::Mismatch,
            x_involved: true,
        };
    }
    let equal = match opts.cmp {
        CmpMode::BitExact => a_val.cmp_key() == b_val.cmp_key(),
        CmpMode::Numeric => numeric_eq(
            a_val,
            a_handle.signed,
            a_handle.eff_width(),
            b_val,
            b_handle.signed,
            b_handle.eff_width(),
        ),
    };
    Verdict {
        mismatch: !equal,
        x_involved: false,
    }
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    from: i64,
    to: i64,
}

fn bounds_before_first_edge(info: &ClockInfo, ts: Option<Timescale>) -> anyhow::Error {
    anyhow::anyhow!(
        "cycle range starts before the first clock edge (first edge at {} (tick {}))",
        fmt_time(ts, info.edges[0]),
        info.edges[0]
    )
}

fn resolve_bounds(
    opts: &Options,
    info: &ClockInfo,
    ts: Option<Timescale>,
    default_to: i64,
) -> Result<Bounds> {
    let from_tick = match opts.from.as_deref() {
        Some(s) => Some(parse_time(s, ts)?),
        None => None,
    };
    let to_tick = match opts.to.as_deref() {
        Some(s) => Some(parse_time(s, ts)?),
        None => None,
    };
    let from = match (opts.from_cycle, from_tick) {
        (Some(a), _) => a,
        (None, Some(t)) => info.cycle_of(t),
        (None, None) => 0,
    };
    let to = match (opts.to_cycle, to_tick) {
        (Some(b), _) => b,
        (None, Some(t)) => info.cycle_of(t),
        (None, None) => default_to,
    };
    if from < 0 {
        return Err(bounds_before_first_edge(info, ts));
    }
    if to < from {
        bail!("empty cycle range {from}..{to}");
    }
    Ok(Bounds { from, to })
}

/// Highest cycle with data for either signal.
fn last_data_cycle(wf: &Waveform, info: &ClockInfo, handles: &[&SigHandle]) -> i64 {
    let tt = wf.time_table();
    let mut last = -1i64;
    for handle in handles {
        if let Some(sig) = handle.signal(wf)
            && let Some(idx) = sig.time_indices().last()
            && let Some(tick) = tt.get(*idx as usize)
        {
            last = last.max(info.cycle_of(*tick));
        }
    }
    last
}

fn is_one(cell: &Cell) -> bool {
    cell.bits() == Some("1")
}

#[allow(clippy::too_many_arguments)]
fn scan_cycles(
    wf: &Waveform,
    info: &ClockInfo,
    a: &SigHandle,
    b: &SigHandle,
    gate: Option<&SigHandle>,
    bounds: Bounds,
    shift: u32,
    opts: &Options,
    collect: bool,
) -> Scan {
    let shift = i64::from(shift);
    let mut scan = Scan::default();
    for c in bounds.from..=bounds.to {
        let bc = c + shift;
        if bc > bounds.to {
            break;
        }
        if let Some(gate) = gate
            && !is_one(&gate.cell_at_cycle(wf, &info.edges, c))
        {
            scan.skipped += 1;
            continue;
        }
        let a_cell = a.cell_at_cycle(wf, &info.edges, c);
        let b_cell = b.cell_at_cycle(wf, &info.edges, bc);
        if a_cell.val.is_none() || b_cell.val.is_none() {
            scan.skipped += 1;
            continue;
        }
        scan.compared += 1;
        let verdict = classify(&a_cell, a, &b_cell, b, opts);
        if verdict.x_involved {
            scan.x_involved += 1;
        }
        if verdict.mismatch {
            scan.mismatches += 1;
            let div = Divergence {
                // reported in the DUT's (B-side) cycle: that is the cycle where
                // the wrong value is observed, and it is shift-independent
                index: bc,
                a_cycle: c,
                b_cycle: bc,
                verdict,
                a_cell: a_cell.clone(),
                b_cell: b_cell.clone(),
            };
            if scan.first_div.is_none() {
                scan.first_div = Some(div.clone());
            }
            if collect {
                scan.divergences.push(div);
            }
        }
    }
    scan
}

struct Events {
    cycles: Vec<i64>,
    cells: Vec<Cell>,
}

fn collect_events(
    wf: &Waveform,
    info: &ClockInfo,
    sig: &SigHandle,
    valid: &SigHandle,
    ready: Option<&SigHandle>,
    bounds: Bounds,
) -> Events {
    let mut events = Events {
        cycles: Vec::new(),
        cells: Vec::new(),
    };
    for c in bounds.from..=bounds.to {
        if !is_one(&valid.cell_at_cycle(wf, &info.edges, c)) {
            continue;
        }
        if let Some(ready) = ready
            && !is_one(&ready.cell_at_cycle(wf, &info.edges, c))
        {
            continue;
        }
        events.cycles.push(c);
        events.cells.push(sig.cell_at_cycle(wf, &info.edges, c));
    }
    events
}

#[allow(clippy::too_many_arguments)]
fn scan_transfers(
    wf: &Waveform,
    info: &ClockInfo,
    a: &SigHandle,
    b: &SigHandle,
    in_valid: &SigHandle,
    in_ready: Option<&SigHandle>,
    out_valid: &SigHandle,
    out_ready: Option<&SigHandle>,
    bounds: Bounds,
    shift: u32,
    opts: &Options,
    collect: bool,
) -> Scan {
    let a_events = collect_events(wf, info, a, in_valid, in_ready, bounds);
    let b_events = collect_events(wf, info, b, out_valid, out_ready, bounds);
    let shift = shift as usize;
    let mut scan = Scan {
        a_events: a_events.cycles.len(),
        b_events: b_events.cycles.len(),
        paired: a_events.cycles.len().min(b_events.cycles.len()),
        unpaired: a_events.cycles.len().abs_diff(b_events.cycles.len()),
        ..Scan::default()
    };
    let units = scan.paired.saturating_sub(shift);
    for k in 0..units {
        let a_cell = &a_events.cells[k];
        let b_cell = &b_events.cells[k + shift];
        let (a_cycle, b_cycle) = (a_events.cycles[k], b_events.cycles[k + shift]);
        if a_cell.val.is_none() || b_cell.val.is_none() {
            scan.skipped += 1;
            continue;
        }
        scan.compared += 1;
        let verdict = classify(a_cell, a, b_cell, b, opts);
        if verdict.x_involved {
            scan.x_involved += 1;
        }
        if verdict.mismatch {
            scan.mismatches += 1;
            let div = Divergence {
                index: k as i64,
                a_cycle,
                b_cycle,
                verdict,
                a_cell: a_cell.clone(),
                b_cell: b_cell.clone(),
            };
            if scan.first_div.is_none() {
                scan.first_div = Some(div.clone());
            }
            if collect {
                scan.divergences.push(div);
            }
        }
    }
    scan
}

// ---------------------------------------------------------------------------
// signal/stream setup shared by align and compare
// ---------------------------------------------------------------------------

fn resolve_optional(
    app: &App,
    h: &wellen::Hierarchy,
    sign: &resolve::SignArgs,
    spec: Option<&String>,
    flag: &str,
) -> Result<Option<SigHandle>> {
    match spec {
        None => Ok(None),
        Some(spec) => {
            let ctx = app.ctx(sign);
            Ok(Some(
                resolve::resolve_sig(h, &ctx, spec).map_err(|e| anyhow::anyhow!("{flag}: {e}"))?,
            ))
        }
    }
}

fn load_all(wf: &mut Waveform, handles: &[&SigHandle]) {
    let ids: Vec<_> = handles.iter().map(|h| h.signal_ref).collect();
    wf.load_signals_multi_threaded(&ids);
}

/// A-side/B-side event streams in transfers mode.
struct Streams {
    in_valid: SigHandle,
    in_ready: Option<SigHandle>,
    out_valid: SigHandle,
    out_ready: Option<SigHandle>,
}

impl Streams {
    fn handles(&self) -> Vec<&SigHandle> {
        let mut out = vec![&self.in_valid, &self.out_valid];
        if let Some(x) = &self.in_ready {
            out.push(x);
        }
        if let Some(x) = &self.out_ready {
            out.push(x);
        }
        out
    }
}

fn resolve_streams(
    app: &App,
    h: &wellen::Hierarchy,
    sign: &resolve::SignArgs,
    opts: &Options,
) -> Result<Option<Streams>> {
    if opts.align != AlignMode::Transfers {
        return Ok(None);
    }
    if opts.gate.is_some() {
        bail!("--gate is not meaningful with --align transfers");
    }
    let in_valid = resolve_optional(app, h, sign, opts.in_valid.as_ref(), "--in-valid")?
        .ok_or_else(|| anyhow::anyhow!("--align transfers requires --in-valid <NAME>"))?;
    let out_valid = resolve_optional(app, h, sign, opts.out_valid.as_ref(), "--out-valid")?
        .ok_or_else(|| anyhow::anyhow!("--align transfers requires --out-valid <NAME>"))?;
    Ok(Some(Streams {
        in_valid,
        in_ready: resolve_optional(app, h, sign, opts.in_ready.as_ref(), "--in-ready")?,
        out_valid,
        out_ready: resolve_optional(app, h, sign, opts.out_ready.as_ref(), "--out-ready")?,
    }))
}

/// Run one scan for the configured align mode.
#[allow(clippy::too_many_arguments)]
fn run_scan(
    wf: &Waveform,
    info: &ClockInfo,
    a: &SigHandle,
    b: &SigHandle,
    gate: Option<&SigHandle>,
    streams: Option<&Streams>,
    bounds: Bounds,
    shift: u32,
    opts: &Options,
    collect: bool,
) -> Scan {
    match opts.align {
        AlignMode::Cycles => scan_cycles(wf, info, a, b, gate, bounds, shift, opts, collect),
        AlignMode::Transfers => {
            let streams = streams.expect("transfers mode requires event streams");
            scan_transfers(
                wf,
                info,
                a,
                b,
                &streams.in_valid,
                streams.in_ready.as_ref(),
                &streams.out_valid,
                streams.out_ready.as_ref(),
                bounds,
                shift,
                opts,
                collect,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// align
// ---------------------------------------------------------------------------

fn parse_shift_range(s: &str) -> Result<(u32, u32)> {
    let body = s.trim();
    let body = body.strip_prefix("0..=").unwrap_or(body);
    let body = body.strip_prefix("..=").unwrap_or(body);
    match body.split_once("..=") {
        Some((a, b)) => Ok((a.trim().parse()?, b.trim().parse()?)),
        None => {
            let v: u32 = body
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("--shifts expects A..=B, got \"{s}\""))?;
            Ok((0, v))
        }
    }
}

pub fn align(app: &App, args: AlignArgs) -> Result<Doc> {
    let path = app.require_file()?;
    let opts = resolve_options(app, &args.common, None);
    let (lo, hi) = match args.shifts.as_deref() {
        Some(s) => parse_shift_range(s)?,
        None => (0, opts.max_shift),
    };
    if hi < lo {
        bail!("empty shift range {lo}..={hi}");
    }

    let mut wf = load::full(path, &load::load_options())?;
    let sign;
    let (a, b) = {
        let h = wf.hierarchy();
        sign = app.sign_args(h)?;
        let ctx = app.ctx(&sign);
        (
            resolve::resolve_sig(h, &ctx, &args.a)?,
            resolve::resolve_sig(h, &ctx, &args.b)?,
        )
    };
    let ts = wf.hierarchy().timescale();
    let info = match clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet)?
    {
        Some(info) => info,
        None => bail!("align needs a clock: do not pass --clock none"),
    };
    let (gate, streams) = {
        let h = wf.hierarchy();
        (
            resolve_optional(app, h, &sign, opts.gate.as_ref(), "--gate")?,
            resolve_streams(app, h, &sign, &opts)?,
        )
    };

    let mut to_load: Vec<&SigHandle> = vec![&a, &b];
    if let Some(g) = &gate {
        to_load.push(g);
    }
    if let Some(s) = &streams {
        to_load.extend(s.handles());
    }
    load_all(&mut wf, &to_load);

    let default_to = match opts.align {
        AlignMode::Cycles => last_data_cycle(&wf, &info, &[&a, &b]),
        AlignMode::Transfers => info.last_cycle(),
    };
    let bounds = resolve_bounds(&opts, &info, ts, default_to.clamp(0, info.last_cycle()))?;

    let mut columns = vec![
        "shift".to_string(),
        "compared".to_string(),
        "mismatches".to_string(),
        "first_divergent_cycle".to_string(),
        "first_divergent_time".to_string(),
    ];
    if opts.align == AlignMode::Transfers {
        columns.push("first_divergent_transfer".to_string());
        columns.push("a_events".to_string());
        columns.push("b_events".to_string());
        columns.push("paired".to_string());
        columns.push("unpaired".to_string());
    }
    let mut table = Table::new(columns);

    let mut best: Option<(u32, u64)> = None;
    let mut best_count = 0usize;
    let mut rows: Vec<(u32, Scan)> = Vec::new();
    for shift in lo..=hi {
        let scan = run_scan(
            &wf,
            &info,
            &a,
            &b,
            gate.as_ref(),
            streams.as_ref(),
            bounds,
            shift,
            &opts,
            false,
        );
        match &best {
            None => {
                best = Some((shift, scan.mismatches));
                best_count = 1;
            }
            Some((_, m)) if scan.mismatches < *m => {
                best = Some((shift, scan.mismatches));
                best_count = 1;
            }
            Some((_, m)) if scan.mismatches == *m => best_count += 1,
            _ => {}
        }
        rows.push((shift, scan));
    }
    let (best_shift, best_mismatches) = best.unwrap_or((lo, 0));
    let ambiguous = best_count > 1;

    for (shift, scan) in &rows {
        let (first_cycle, first_time, first_transfer) = match &scan.first_div {
            Some(d) => (
                d.b_cycle.to_string(),
                format_time(info.edges[d.b_cycle as usize], ts),
                d.index.to_string(),
            ),
            None => ("-".to_string(), "-".to_string(), "-".to_string()),
        };
        let mut text = vec![
            shift.to_string(),
            scan.compared.to_string(),
            scan.mismatches.to_string(),
            first_cycle,
            first_time.clone(),
        ];
        let mut row = json!({
            "shift": shift,
            "compared": scan.compared,
            "mismatches": scan.mismatches,
            "x_involved": scan.x_involved,
            "skipped": scan.skipped,
            "first_divergent_cycle": scan.first_div.as_ref().map(|d| d.b_cycle),
            "first_divergent_a_cycle": scan.first_div.as_ref().map(|d| d.a_cycle),
            "first_divergent_time": scan.first_div.as_ref().map(|_| first_time),
        });
        if opts.align == AlignMode::Transfers {
            text.push(first_transfer);
            text.push(scan.a_events.to_string());
            text.push(scan.b_events.to_string());
            text.push(scan.paired.to_string());
            text.push(scan.unpaired.to_string());
            row["first_divergent_transfer"] = json!(scan.first_div.as_ref().map(|d| d.index));
            row["a_events"] = json!(scan.a_events);
            row["b_events"] = json!(scan.b_events);
            row["paired"] = json!(scan.paired);
            row["unpaired"] = json!(scan.unpaired);
        }
        table.push(text, row);
    }

    let best_scan = rows
        .iter()
        .find(|(shift, _)| *shift == best_shift)
        .map(|(_, scan)| scan.clone());
    if let Some(scan) = &best_scan
        && opts.align == AlignMode::Transfers
        && scan.unpaired > 0
    {
        app.note(&format!(
            "align: transfers mode: a_events={} b_events={} unpaired={} (expected to equal the pipeline depth; \
             a difference between runs of the same design indicates swallowed or duplicated transfers)",
            scan.a_events, scan.b_events, scan.unpaired
        ));
    }

    let mut doc = Doc::new()
        .field(Field::s("file", path.display().to_string()))
        .field(Field::s("clock", info.full_name.clone()))
        .field(Field::s("edge", info.edge.name()))
        .field(Field::s("align", opts.align.name()))
        .field(Field::s("cmp", opts.cmp.name()))
        .field(Field::s("x", opts.x.name()))
        .field(Field::n("from_cycle", bounds.from))
        .field(Field::n("to_cycle", bounds.to))
        .field(Field::s("a", a.full_name.clone()))
        .field(Field::s("b", b.full_name.clone()))
        .table(table)
        .trailer_field(Field::u("best_shift", u64::from(best_shift)))
        .trailer_field(Field::u("best_mismatches", best_mismatches))
        .trailer_field(Field::b("ambiguous", ambiguous))
        .trailer_field(Field::s("mode", opts.align.name()))
        .header_record(true)
        .exit(if best_mismatches == 0 { 0 } else { 1 });
    if let Some(scan) = &best_scan {
        doc = doc
            .trailer_field(Field::u("compared", scan.compared))
            .trailer_field(Field::u("skipped", scan.skipped))
            .trailer_field(Field::u("x_involved", scan.x_involved));
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// compare
// ---------------------------------------------------------------------------

fn parse_map(s: &str) -> Result<(String, String, Option<u32>)> {
    let (a, rest) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--map expects A=B or A=B:SHIFT, got \"{s}\""))?;
    let a = a.trim();
    if a.is_empty() {
        bail!("--map expects A=B or A=B:SHIFT, got \"{s}\"");
    }
    let rest = rest.trim();
    match rest.rsplit_once(':') {
        Some((head, tail))
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) && !head.is_empty() =>
        {
            Ok((a.to_string(), head.to_string(), Some(tail.parse()?)))
        }
        _ => Ok((a.to_string(), rest.to_string(), None)),
    }
}

struct PairResult {
    scan: Scan,
    shift: u32,
    ambiguous: Option<bool>,
    auto_mismatches: Option<u64>,
}

pub fn compare(app: &App, args: CompareArgs) -> Result<Doc> {
    let path = app.require_file()?;
    let named = match args.check.as_deref() {
        None => None,
        Some(name) => Some(app.cfg.check(name).cloned().ok_or_else(|| {
            let names = app.cfg.check_names();
            if names.is_empty() {
                anyhow::anyhow!("no check \"{name}\": the config defines no [checks]")
            } else {
                anyhow::anyhow!("no check \"{name}\"; checks: {}", names.join(", "))
            }
        })?),
    };
    let opts = resolve_options(app, &args.common, named.as_ref());

    // pairs: the named check's pair first, CLI --map entries appended
    let mut pairs: Vec<(String, String, Option<u32>)> = Vec::new();
    if let Some(check) = &named {
        match (&check.a, &check.b) {
            (Some(a), Some(b)) => pairs.push((a.clone(), b.clone(), check.shift)),
            (None, None) => {}
            _ => bail!(
                "check \"{}\" needs both a and b",
                args.check.as_deref().unwrap_or("")
            ),
        }
    }
    for m in &args.map {
        pairs.push(parse_map(m)?);
    }
    if let Some(shift) = args.shift {
        for pair in &mut pairs {
            pair.2 = Some(shift);
        }
    }
    if pairs.is_empty() {
        bail!("no comparison pairs: pass --map A=B[:SHIFT] (repeatable) or --check NAME");
    }

    let mut wf = load::full(path, &load::load_options())?;
    let sign;
    let probe_handles = {
        let h = wf.hierarchy();
        sign = app.sign_args(h)?;
        if args.probe.is_empty() {
            Vec::new()
        } else {
            let specs = app.expand_signals(&args.probe)?;
            app.resolve_all(h, &sign, &specs)?
        }
    };
    let ts = wf.hierarchy().timescale();

    let info = match clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet)?
    {
        Some(info) => info,
        None => bail!("compare needs a clock: do not pass --clock none"),
    };

    let (gate, streams) = {
        let h = wf.hierarchy();
        (
            resolve_optional(app, h, &sign, opts.gate.as_ref(), "--gate")?,
            resolve_streams(app, h, &sign, &opts)?,
        )
    };

    let resolved_pairs: Vec<(SigHandle, SigHandle, Option<u32>)> = {
        let h = wf.hierarchy();
        let ctx = app.ctx(&sign);
        let mut out = Vec::with_capacity(pairs.len());
        for (a, b, shift) in &pairs {
            let a_handle = resolve::resolve_sig(h, &ctx, a)?;
            let b_handle = resolve::resolve_sig(h, &ctx, b)?;
            if a_handle.full_name == b_handle.full_name && a == b {
                app.note(&format!(
                    "compare: pair {}={} compares a signal with itself",
                    a_handle.full_name, b_handle.full_name
                ));
            }
            out.push((a_handle, b_handle, *shift));
        }
        out
    };

    let mut to_load: Vec<&SigHandle> = Vec::new();
    for (a, b, _) in &resolved_pairs {
        to_load.push(a);
        to_load.push(b);
    }
    if let Some(g) = &gate {
        to_load.push(g);
    }
    if let Some(s) = &streams {
        to_load.extend(s.handles());
    }
    for p in &probe_handles {
        to_load.push(p);
    }
    load_all(&mut wf, &to_load);

    let all_pairs: Vec<&SigHandle> = resolved_pairs.iter().flat_map(|(a, b, _)| [a, b]).collect();
    let default_to = match opts.align {
        AlignMode::Cycles => last_data_cycle(&wf, &info, &all_pairs),
        AlignMode::Transfers => info.last_cycle(),
    };
    let bounds = resolve_bounds(&opts, &info, ts, default_to.clamp(0, info.last_cycle()))?;

    // shift decision per pair
    let mut shifts: Vec<u32> = Vec::with_capacity(resolved_pairs.len());
    let mut auto_info: Vec<Option<(bool, u64)>> = Vec::with_capacity(resolved_pairs.len());
    for (a, b, shift) in &resolved_pairs {
        if let Some(s) = shift {
            shifts.push(*s);
            auto_info.push(None);
            continue;
        }
        if args.auto_shift {
            let mut best: Option<(u32, u64)> = None;
            let mut count = 0usize;
            for s in 0..=opts.max_shift {
                let scan = run_scan(
                    &wf,
                    &info,
                    a,
                    b,
                    gate.as_ref(),
                    streams.as_ref(),
                    bounds,
                    s,
                    &opts,
                    false,
                );
                match &best {
                    None => {
                        best = Some((s, scan.mismatches));
                        count = 1;
                    }
                    Some((_, m)) if scan.mismatches < *m => {
                        best = Some((s, scan.mismatches));
                        count = 1;
                    }
                    Some((_, m)) if scan.mismatches == *m => count += 1,
                    _ => {}
                }
            }
            let (s, m) = best.unwrap_or((0, 0));
            shifts.push(s);
            auto_info.push(Some((count > 1, m)));
            continue;
        }
        if opts.align == AlignMode::Transfers {
            shifts.push(0);
            auto_info.push(None);
            continue;
        }
        bail!(
            "pair {}={}: specify a shift (A=B:3) or pass --auto-shift",
            a.full_name,
            b.full_name
        );
    }

    // final scan per pair (with divergence rows)
    let mut results: Vec<PairResult> = Vec::new();
    for (i, (a, b, _)) in resolved_pairs.iter().enumerate() {
        let scan = run_scan(
            &wf,
            &info,
            a,
            b,
            gate.as_ref(),
            streams.as_ref(),
            bounds,
            shifts[i],
            &opts,
            true,
        );
        let (ambiguous, auto_mismatches) = match auto_info[i] {
            Some((amb, m)) => (Some(amb), Some(m)),
            None => (None, None),
        };
        results.push(PairResult {
            scan,
            shift: shifts[i],
            ambiguous,
            auto_mismatches,
        });
    }

    // merged divergence rows
    struct RowRef {
        pair: usize,
        div: Divergence,
    }
    let mut rows: Vec<RowRef> = Vec::new();
    for (i, r) in results.iter().enumerate() {
        for d in &r.scan.divergences {
            rows.push(RowRef {
                pair: i,
                div: d.clone(),
            });
        }
    }
    rows.sort_by(|x, y| x.div.index.cmp(&y.div.index).then(x.pair.cmp(&y.pair)));
    if args.at_edges_only {
        rows.retain(|r| {
            let a_edge = info.edges.get(r.div.a_cycle as usize).copied();
            let b_edge = info.edges.get(r.div.b_cycle as usize).copied();
            r.div.a_cell.change_tick == a_edge && r.div.b_cell.change_tick == b_edge
        });
    }
    if args.first_only {
        rows.truncate(1);
    }

    let mut columns = vec![
        if opts.align == AlignMode::Transfers {
            "transfer"
        } else {
            "cycle"
        }
        .to_string(),
        "time".to_string(),
        "a_cycle".to_string(),
        "b_cycle".to_string(),
        "pair".to_string(),
        "a".to_string(),
        "b".to_string(),
        "cmp".to_string(),
        "a_at".to_string(),
        "b_at".to_string(),
    ];
    for p in &probe_handles {
        columns.push(format!("probe_{}", p.full_name));
    }

    let max_rows = app.max_rows(20);
    let mut table = Table::new(columns);
    let mut omitted = 0usize;
    for (i, row) in rows.iter().enumerate() {
        if let Some(max) = max_rows
            && table.rows.len() >= max
        {
            omitted = rows.len() - i;
            break;
        }
        let (a_handle, b_handle, _) = &resolved_pairs[row.pair];
        let time = format_time(info.edges[row.div.b_cycle as usize], ts);
        let a_at = row
            .div
            .a_cell
            .change_tick
            .map(|t| format_time(t, ts))
            .unwrap_or_else(|| "-".to_string());
        let b_at = row
            .div
            .b_cell
            .change_tick
            .map(|t| format_time(t, ts))
            .unwrap_or_else(|| "-".to_string());
        let mut text = vec![
            row.div.index.to_string(),
            time.clone(),
            row.div.a_cycle.to_string(),
            row.div.b_cycle.to_string(),
            row.pair.to_string(),
            row.div.a_cell.text(a_handle.radix, a_handle.signed),
            row.div.b_cell.text(b_handle.radix, b_handle.signed),
            row.div.verdict.label().to_string(),
            a_at,
            b_at,
        ];
        let mut probes = Map::new();
        for p in &probe_handles {
            let cell = p.cell_at_cycle(&wf, &info.edges, row.div.a_cycle);
            text.push(cell.text(p.radix, p.signed));
            probes.insert(p.full_name.clone(), Value::Object(value_json(&cell, p)));
        }
        let mut json_row = Map::new();
        if opts.align == AlignMode::Transfers {
            json_row.insert("transfer".into(), row.div.index.into());
        } else {
            json_row.insert("cycle".into(), row.div.index.into());
        }
        json_row.insert("a_cycle".into(), row.div.a_cycle.into());
        json_row.insert("b_cycle".into(), row.div.b_cycle.into());
        json_row.insert("time".into(), time.into());
        json_row.insert("pair".into(), row.pair.into());
        json_row.insert("a_signal".into(), a_handle.full_name.clone().into());
        json_row.insert("b_signal".into(), b_handle.full_name.clone().into());
        json_row.insert(
            "a".into(),
            Value::Object(value_json(&row.div.a_cell, a_handle)),
        );
        json_row.insert(
            "b".into(),
            Value::Object(value_json(&row.div.b_cell, b_handle)),
        );
        json_row.insert("cmp".into(), row.div.verdict.label().into());
        json_row.insert(
            "a_change_time".into(),
            row.div
                .a_cell
                .change_tick
                .map(|t| Value::from(format_time(t, ts)))
                .unwrap_or(Value::Null),
        );
        json_row.insert(
            "b_change_time".into(),
            row.div
                .b_cell
                .change_tick
                .map(|t| Value::from(format_time(t, ts)))
                .unwrap_or(Value::Null),
        );
        json_row.insert("pre_clock".into(), (row.div.b_cycle < 0).into());
        if !probes.is_empty() {
            json_row.insert("probe".into(), Value::Object(probes));
        }
        table.push(text, Value::Object(json_row));
    }

    // per-pair summary
    let mut pair_columns = vec![
        "pair".to_string(),
        "a".to_string(),
        "b".to_string(),
        "shift".to_string(),
        "align".to_string(),
        "compared".to_string(),
        "mismatches".to_string(),
        "x_involved".to_string(),
        "skipped".to_string(),
        "first_divergent_cycle".to_string(),
        "first_divergent_time".to_string(),
        "status".to_string(),
    ];
    if opts.align == AlignMode::Transfers {
        for c in ["a_events", "b_events", "paired", "unpaired"] {
            pair_columns.push(c.to_string());
        }
    }
    let mut pair_table = Table::named("pairs", pair_columns);
    let mut total_mismatches = 0u64;
    for (i, r) in results.iter().enumerate() {
        total_mismatches += r.scan.mismatches;
        let (a_handle, b_handle, _) = &resolved_pairs[i];
        let first_time = r
            .scan
            .first_div
            .as_ref()
            .map(|d| format_time(info.edges[d.b_cycle as usize], ts));
        let mut text = vec![
            i.to_string(),
            a_handle.full_name.clone(),
            b_handle.full_name.clone(),
            r.shift.to_string(),
            opts.align.name().to_string(),
            r.scan.compared.to_string(),
            r.scan.mismatches.to_string(),
            r.scan.x_involved.to_string(),
            r.scan.skipped.to_string(),
            r.scan
                .first_div
                .as_ref()
                .map(|d| d.b_cycle.to_string())
                .unwrap_or_else(|| "-".to_string()),
            first_time.clone().unwrap_or_else(|| "-".to_string()),
            if r.scan.mismatches == 0 {
                "ok"
            } else {
                "divergent"
            }
            .to_string(),
        ];
        let mut json_row = json!({
            "pair": i,
            "a": a_handle.full_name,
            "b": b_handle.full_name,
            "shift": r.shift,
            "align": opts.align.name(),
            "cmp": opts.cmp.name(),
            "x": opts.x.name(),
            "compared": r.scan.compared,
            "mismatches": r.scan.mismatches,
            "x_involved": r.scan.x_involved,
            "skipped": r.scan.skipped,
            "first_divergent_cycle": r.scan.first_div.as_ref().map(|d| d.b_cycle),
            "first_divergent_a_cycle": r.scan.first_div.as_ref().map(|d| d.a_cycle),
            "first_divergent_time": first_time,
            "status": if r.scan.mismatches == 0 { "ok" } else { "divergent" },
        });
        if opts.align == AlignMode::Transfers {
            text.push(r.scan.a_events.to_string());
            text.push(r.scan.b_events.to_string());
            text.push(r.scan.paired.to_string());
            text.push(r.scan.unpaired.to_string());
            json_row["a_events"] = json!(r.scan.a_events);
            json_row["b_events"] = json!(r.scan.b_events);
            json_row["paired"] = json!(r.scan.paired);
            json_row["unpaired"] = json!(r.scan.unpaired);
        }
        if let Some(amb) = r.ambiguous {
            json_row["auto_shift_ambiguous"] = json!(amb);
            json_row["auto_shift_mismatches"] = json!(r.auto_mismatches);
        }
        pair_table.push(text, json_row);
    }

    if opts.align == AlignMode::Transfers {
        for (i, r) in results.iter().enumerate() {
            if r.scan.unpaired > 0 {
                let (a_handle, b_handle, _) = &resolved_pairs[i];
                app.note(&format!(
                    "compare: transfers mode: pair {}={}: a_events={} b_events={} unpaired={} (expected to equal \
                     the pipeline depth; a difference between runs of the same design indicates swallowed or duplicated transfers)",
                    a_handle.full_name, b_handle.full_name, r.scan.a_events, r.scan.b_events, r.scan.unpaired
                ));
            }
        }
    }

    let mut doc = Doc::new()
        .field(Field::s("file", path.display().to_string()))
        .field(Field::s("clock", info.full_name.clone()))
        .field(Field::s("edge", info.edge.name()))
        .field(Field::s("align", opts.align.name()))
        .field(Field::s("cmp", opts.cmp.name()))
        .field(Field::s("x", opts.x.name()))
        .field(Field::n("from_cycle", bounds.from))
        .field(Field::n("to_cycle", bounds.to))
        .field(Field::u("pairs", results.len() as u64))
        .table(table)
        .table(pair_table)
        .trailer_field(Field::u("mismatches", total_mismatches))
        .trailer_field(Field::s(
            "status",
            if total_mismatches == 0 {
                "ok"
            } else {
                "divergent"
            },
        ))
        .header_record(true)
        .exit(if total_mismatches == 0 { 0 } else { 1 });
    if omitted > 0 {
        app.note(&format!(
            "compare: showing the first {} of {} divergent rows (use --max-rows 0 for all)",
            max_rows.unwrap_or(rows.len()),
            rows.len()
        ));
        doc = doc
            .trailer_field(Field::b("truncated", true))
            .trailer_field(Field::u("omitted", omitted as u64));
    }
    Ok(doc)
}

fn value_json(cell: &Cell, handle: &SigHandle) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("signal".into(), handle.full_name.clone().into());
    m.insert(
        "value".into(),
        cell.val
            .as_ref()
            .map(|v| v.render(handle.radix, handle.signed).unwrap_or_default())
            .into(),
    );
    m.insert(
        "bits".into(),
        cell.val
            .as_ref()
            .map(|v| v.json_bits())
            .unwrap_or(Value::Null),
    );
    m.insert(
        "has_x".into(),
        cell.val.as_ref().is_some_and(|v| v.has_xz()).into(),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_parsing() {
        assert_eq!(
            parse_map("tb_top.ref_y=tb_top.q3").unwrap(),
            ("tb_top.ref_y".into(), "tb_top.q3".into(), None)
        );
        assert_eq!(
            parse_map("tb_top.ref_y=tb_top.q3:3").unwrap(),
            ("tb_top.ref_y".into(), "tb_top.q3".into(), Some(3))
        );
        assert_eq!(
            parse_map("a=b[15:8]").unwrap(),
            ("a".into(), "b[15:8]".into(), None)
        );
        assert_eq!(
            parse_map("a=b[15:8]:2").unwrap(),
            ("a".into(), "b[15:8]".into(), Some(2))
        );
        assert!(parse_map("a").is_err());
    }

    #[test]
    fn shift_range_parsing() {
        assert_eq!(parse_shift_range("0..=8").unwrap(), (0, 8));
        assert_eq!(parse_shift_range("2..=5").unwrap(), (2, 5));
        assert_eq!(parse_shift_range("4").unwrap(), (0, 4));
    }

    #[test]
    fn verdict_labels() {
        assert_eq!(
            Verdict {
                mismatch: false,
                x_involved: false
            }
            .label(),
            "ok"
        );
        assert_eq!(
            Verdict {
                mismatch: true,
                x_involved: false
            }
            .label(),
            "mismatch"
        );
        assert_eq!(
            Verdict {
                mismatch: true,
                x_involved: true
            }
            .label(),
            "x-involved"
        );
    }

    #[test]
    fn numeric_extension() {
        // unsigned 253 vs signed -3 (8 bit) are different values
        assert!(!numeric_eq(
            &Val::Bits("11111101".into()),
            false,
            8,
            &Val::Bits("11111101".into()),
            true,
            8
        ));
        // 9-bit unsigned 56 vs 8-bit unsigned 56 extend to the same value
        assert!(numeric_eq(
            &Val::Bits("000111000".into()),
            false,
            9,
            &Val::Bits("00111000".into()),
            false,
            8
        ));
        // sign extension: 8-bit -3 vs 9-bit -3
        assert!(numeric_eq(
            &Val::Bits("11111101".into()),
            true,
            8,
            &Val::Bits("111111101".into()),
            true,
            9
        ));
    }
}
