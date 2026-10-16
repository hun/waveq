//! `align` and `compare`: shift scanning, pair expansion, transfer indexing and
//! divergence classification — the semantic core of the
//! golden-vs-pipelined and composition-debugging workflows.
//!
//! Both commands work on [`Sides`]: side A is the primary dump (`-f`), side B is
//! either the same dump or a second one (`--b-file`). Units are numbered in A's
//! logical cycle space (see `sides.rs`); divergence rows are keyed by the
//! **DUT (B-side) cycle** where the wrong value is observed and carry both
//! cycles explicitly.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use wellen::simple::Waveform;
use wellen::{Hierarchy, SignalEncoding, SignalRef, Timescale, VarRef};

use crate::cli::ProbeAt;
use crate::clock::ClockInfo;
use crate::config::CheckCfg;
use crate::expect::{ExpectCache, ExpectTable};
use crate::format::{AlignMode, CmpMode, XMode, format_time, parse_time};
use crate::load;
use crate::output::{Doc, Field, Table};
use crate::resolve::{self, SigHandle};
use crate::sample::{Cell, Val};
use crate::sides::{
    Side, SideView, Sides, hierarchy_of, resolve_flag, resolve_on, resolve_origin,
    resolve_side_clock, side_b_origin,
};

use super::{App, fmt_time};

// ---------------------------------------------------------------------------
// options
// ---------------------------------------------------------------------------

/// CLI inputs shared by `align` and `compare`.
#[derive(Debug, Clone, Default)]
pub struct CommonCli {
    pub align: Option<AlignMode>,
    pub cmp: Option<CmpMode>,
    pub x: Option<XMode>,
    pub max_shift: Option<u32>,
    pub max_delta: Option<i128>,
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

/// Fully resolved comparison options (`CLI > named check > [compare] > built-in`).
#[derive(Debug, Clone)]
pub struct Options {
    pub align: AlignMode,
    pub cmp: CmpMode,
    pub x: XMode,
    pub max_shift: u32,
    pub max_delta: Option<i128>,
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
    /// `--pair-scopes A,B`: pair every signal under two scopes by name.
    pub pair_scopes: Option<String>,
    /// `--pair-by leaf|path` for scope/glob pairing.
    pub pair_by: Option<String>,
    /// `--map-glob GLOB_A=GLOB_B` (repeatable), paired by name.
    pub map_glob: Vec<String>,
    /// Add delta / bits_differ columns to divergence rows.
    pub delta: bool,
    /// Override where `--probe` signals are sampled.
    pub probe_at: Option<ProbeAt>,
    pub common: CommonCli,
}

impl Options {
    /// Numeric interpretation is needed when comparing numbers or deltas.
    pub fn numeric(&self) -> bool {
        self.cmp == CmpMode::Numeric || self.max_delta.is_some()
    }

    pub fn cmp_name(&self) -> &'static str {
        if self.cmp == CmpMode::BitExact && self.max_delta.is_some() {
            "bit-exact (+numeric delta)"
        } else {
            self.cmp.name()
        }
    }
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
        max_delta: pick(
            cli.max_delta,
            named.and_then(|c| c.max_delta).map(i128::from),
            cfg.max_delta.map(i128::from),
        ),
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

/// A numeric difference, kept exact for integers.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DeltaVal {
    Int(i128),
    Real(f64),
}

impl DeltaVal {
    fn abs(self) -> DeltaVal {
        match self {
            DeltaVal::Int(v) => DeltaVal::Int(v.abs()),
            DeltaVal::Real(v) => DeltaVal::Real(v.abs()),
        }
    }

    fn exceeds(self, bound: i128) -> bool {
        match self {
            DeltaVal::Int(v) => v.abs() > bound,
            DeltaVal::Real(v) => v.abs() > bound as f64,
        }
    }

    fn to_json(self) -> Value {
        match self {
            DeltaVal::Int(v) => json!(v),
            DeltaVal::Real(v) => json!(v),
        }
    }

    fn to_text(self) -> String {
        match self {
            DeltaVal::Int(v) => v.to_string(),
            DeltaVal::Real(v) => crate::sample::format_real(v),
        }
    }

    fn max(self, other: DeltaVal) -> DeltaVal {
        let a = self.abs();
        let b = other.abs();
        match (a, b) {
            (DeltaVal::Int(x), DeltaVal::Int(y)) => DeltaVal::Int(x.max(y)),
            (DeltaVal::Real(x), DeltaVal::Real(y)) => DeltaVal::Real(x.max(y)),
            (DeltaVal::Int(x), DeltaVal::Real(y)) => DeltaVal::Real((x as f64).max(y)),
            (DeltaVal::Real(x), DeltaVal::Int(y)) => DeltaVal::Real(x.max(y as f64)),
        }
    }
}

#[derive(Debug, Clone)]
struct Verdict {
    label: &'static str,
    mismatch: bool,
    x_involved: bool,
    delta: Option<DeltaVal>,
    /// `a xor b` as hex, when both sides are bit-vectors of equal width.
    bits_differ: Option<String>,
    /// Number of differing bits (0 when widths differ or x/z is involved).
    diff_bits: u32,
}

impl Verdict {
    fn plain(label: &'static str, mismatch: bool, x_involved: bool) -> Verdict {
        Verdict {
            label,
            mismatch,
            x_involved,
            delta: None,
            bits_differ: None,
            diff_bits: 0,
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
    max_abs_delta: Option<DeltaVal>,
    max_bits_differ: u32,
    /// units where a numeric delta was available
    delta_units: u64,
}

impl Scan {
    fn account(&mut self, verdict: &Verdict) {
        if let Some(delta) = verdict.delta {
            self.delta_units += 1;
            self.max_abs_delta = Some(match self.max_abs_delta {
                Some(prev) => prev.max(delta),
                None => delta.abs(),
            });
        }
        self.max_bits_differ = self.max_bits_differ.max(verdict.diff_bits);
    }
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

#[allow(clippy::too_many_arguments)]
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

/// Numeric difference and bit-difference mask, when they are well defined.
fn numeric_detail(
    a: &Val,
    a_handle: &SigHandle,
    b: &Val,
    b_handle: &SigHandle,
) -> (Option<DeltaVal>, Option<String>, u32) {
    match (a, b) {
        (Val::Bits(ab), Val::Bits(bb)) => {
            if ab.len() != bb.len() {
                // width mismatch: the delta still says *how* far off the value is
                let av = bits_to_i128(ab, a_handle.signed, a_handle.eff_width());
                let bv = bits_to_i128(bb, b_handle.signed, b_handle.eff_width());
                return (Some(DeltaVal::Int(bv - av)), None, 0);
            }
            let xor: String = ab
                .chars()
                .zip(bb.chars())
                .map(|(x, y)| if x == y { '0' } else { '1' })
                .collect();
            let diff_bits = xor.chars().filter(|c| *c == '1').count() as u32;
            let mask = crate::format::render(&xor, crate::format::Radix::Hex, false).ok();
            let av = bits_to_i128(ab, a_handle.signed, a_handle.eff_width());
            let bv = bits_to_i128(bb, b_handle.signed, b_handle.eff_width());
            (Some(DeltaVal::Int(bv - av)), mask, diff_bits)
        }
        (Val::Real(x), Val::Real(y)) => (Some(DeltaVal::Real(y - x)), None, 0),
        _ => (None, None, 0),
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
        _ => return Verdict::plain("ok", false, false),
    };
    let x_involved = a_val.has_xz() || b_val.has_xz();
    if x_involved {
        let mismatch = opts.x == XMode::Mismatch;
        return Verdict::plain(if mismatch { "x-involved" } else { "ok" }, mismatch, true);
    }
    let (delta, bits_differ, diff_bits) = numeric_detail(a_val, a_handle, b_val, b_handle);
    if let Some(bound) = opts.max_delta
        && let Some(delta) = delta
    {
        let over = delta.exceeds(bound);
        return Verdict {
            label: if over { "delta-exceeded" } else { "ok" },
            mismatch: over,
            x_involved: false,
            delta: Some(delta),
            bits_differ,
            diff_bits,
        };
    }
    let equal = match opts.cmp {
        CmpMode::Numeric => numeric_eq(
            a_val,
            a_handle.signed,
            a_handle.eff_width(),
            b_val,
            b_handle.signed,
            b_handle.eff_width(),
        ),
        CmpMode::BitExact => a_val.cmp_key() == b_val.cmp_key(),
    };
    Verdict {
        label: if equal { "ok" } else { "mismatch" },
        mismatch: !equal,
        x_involved: false,
        delta,
        bits_differ,
        diff_bits,
    }
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    from: i64,
    to: i64,
}

fn bounds_before_first_edge(view: &SideView<'_>, ts: Option<Timescale>) -> anyhow::Error {
    let first_local = (-view.origin).max(0);
    let tick = view.tick_of(0).max(view.tick_of(-view.origin)).unwrap_or(0);
    anyhow::anyhow!(
        "cycle range starts before the first clock edge of side {} (cycle {} at {} (tick {})); \
         use --anchor or --cycle-offset to redefine cycle 0 (first cycle here is {})",
        view.side.name(),
        view.local_of(first_local),
        fmt_time(ts, tick),
        tick,
        first_local
    )
}

fn resolve_bounds(
    opts: &Options,
    view: &SideView<'_>,
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
    let logical_of_tick = |tick: u64| view.logical_of(view.clock.cycle_of(tick));
    let from = match (opts.from_cycle, from_tick) {
        (Some(a), _) => a,
        (None, Some(t)) => logical_of_tick(t),
        (None, None) => 0,
    };
    let to = match (opts.to_cycle, to_tick) {
        (Some(b), _) => b,
        (None, Some(t)) => logical_of_tick(t),
        (None, None) => default_to,
    };
    // a negative origin (e.g. `--cycle-offset -2040` reading a cut window)
    // means the file's first existing logical cycle is `-origin`
    let first_logical = 0.max(-view.origin);
    if from < first_logical {
        return Err(bounds_before_first_edge(view, ts));
    }
    if to < from {
        bail!("empty cycle range {from}..{to}");
    }
    Ok(Bounds { from, to })
}

#[allow(clippy::too_many_arguments)]
fn scan_cycles(
    sides: &Sides<'_>,
    a: &SigHandle,
    b: &BProvider,
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
            && !sides.is_one(gate, c)
        {
            scan.skipped += 1;
            continue;
        }
        let a_cell = sides.cell(a, c);
        let b_cell = b.cell(sides, bc);
        if a_cell.val.is_none() || b_cell.val.is_none() {
            scan.skipped += 1;
            continue;
        }
        scan.compared += 1;
        let verdict = classify(&a_cell, a, &b_cell, b.handle(), opts);
        scan.account(&verdict);
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
                a_cell,
                b_cell,
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
    sides: &Sides<'_>,
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
        if !sides.is_one(valid, c) {
            continue;
        }
        if let Some(ready) = ready
            && !sides.is_one(ready, c)
        {
            continue;
        }
        events.cycles.push(c);
        events.cells.push(sides.cell(sig, c));
    }
    events
}

#[allow(clippy::too_many_arguments)]
fn collect_events_b(
    sides: &Sides<'_>,
    sig: &BProvider,
    valid: &SigHandle,
    ready: Option<&SigHandle>,
    bounds: Bounds,
) -> Events {
    let mut events = Events {
        cycles: Vec::new(),
        cells: Vec::new(),
    };
    for c in bounds.from..=bounds.to {
        if !sides.is_one(valid, c) {
            continue;
        }
        if let Some(ready) = ready
            && !sides.is_one(ready, c)
        {
            continue;
        }
        events.cycles.push(c);
        events.cells.push(sig.cell(sides, c));
    }
    events
}

#[allow(clippy::too_many_arguments)]
fn scan_transfers(
    sides: &Sides<'_>,
    a: &SigHandle,
    b: &BProvider,
    in_valid: &SigHandle,
    in_ready: Option<&SigHandle>,
    out_valid: &SigHandle,
    out_ready: Option<&SigHandle>,
    bounds: Bounds,
    shift: u32,
    opts: &Options,
    collect: bool,
) -> Scan {
    let a_events = collect_events(sides, a, in_valid, in_ready, bounds);
    let b_events = collect_events_b(sides, b, out_valid, out_ready, bounds);
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
        let verdict = classify(a_cell, a, b_cell, b.handle(), opts);
        scan.account(&verdict);
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
// side loading and handle resolution
// ---------------------------------------------------------------------------

/// Load the needed signals into whichever waveform owns each handle.
fn load_handles(wf_a: &mut Waveform, wf_b: Option<&mut Waveform>, handles: &[&SigHandle]) {
    let mut ids_a = Vec::new();
    let mut ids_b = Vec::new();
    for handle in handles {
        match handle.side {
            Side::A => ids_a.push(handle.signal_ref),
            Side::B => ids_b.push(handle.signal_ref),
        }
    }
    if !ids_a.is_empty() {
        wf_a.load_signals_multi_threaded(&ids_a);
    }
    if !ids_b.is_empty() {
        match wf_b {
            Some(wb) => wb.load_signals_multi_threaded(&ids_b),
            None => wf_a.load_signals_multi_threaded(&ids_b),
        }
    }
}

/// Where side B's values come from: a signal, or a file of expected values.
#[derive(Clone)]
enum BProvider {
    Signal(SigHandle),
    Expect {
        table: std::rc::Rc<ExpectTable>,
        /// Signal handle carrying width/signedness/radix for rendering.
        meta: SigHandle,
        spec: String,
    },
}

impl BProvider {
    fn cell(&self, sides: &Sides<'_>, logical: i64) -> Cell {
        match self {
            BProvider::Signal(handle) => sides.cell(handle, logical),
            BProvider::Expect { table, meta, .. } => {
                match table.value(logical, meta.eff_width(), meta.signed) {
                    Ok(Some(val)) => Cell {
                        val: Some(val),
                        change_tick: None,
                    },
                    _ => Cell::default(),
                }
            }
        }
    }

    /// Metadata used for rendering and comparison.
    fn handle(&self) -> &SigHandle {
        match self {
            BProvider::Signal(handle) => handle,
            BProvider::Expect { meta, .. } => meta,
        }
    }

    fn label(&self) -> String {
        match self {
            BProvider::Signal(handle) => handle.full_name.clone(),
            BProvider::Expect { spec, .. } => spec.clone(),
        }
    }

    fn signal(&self) -> Option<&SigHandle> {
        match self {
            BProvider::Signal(handle) => Some(handle),
            BProvider::Expect { .. } => None,
        }
    }
}

/// Everything needed to compare: both waveforms, clocks, origins.
struct Ctx<'w> {
    wf_a: &'w Waveform,
    wf_b: &'w Waveform,
    shared: bool,
    clock_a: &'w ClockInfo,
    clock_b: &'w ClockInfo,
    origin_a: i64,
    origin_b: i64,
    path_a: std::path::PathBuf,
    path_b: std::path::PathBuf,
}

impl<'w> Ctx<'w> {
    fn sides(&self) -> Sides<'w> {
        Sides {
            a: SideView {
                side: Side::A,
                wf: self.wf_a,
                clock: self.clock_a,
                origin: self.origin_a,
                path: self.path_a.clone(),
                shared: true,
            },
            b: SideView {
                side: Side::B,
                wf: self.wf_b,
                clock: self.clock_b,
                origin: self.origin_b,
                path: self.path_b.clone(),
                shared: self.shared,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// pairing helpers (--pair-scopes / --map-glob / [compare.pairs])
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairBy {
    Leaf,
    Path,
}

impl PairBy {
    fn parse(value: Option<&str>) -> Result<PairBy> {
        match value {
            None | Some("leaf") => Ok(PairBy::Leaf),
            Some("path") => Ok(PairBy::Path),
            Some(other) => bail!("--pair-by {other}: only \"leaf\" and \"path\" are supported"),
        }
    }
}

fn leaf_of(full_name: &str) -> &str {
    match full_name.rsplit_once('.') {
        Some((_, leaf)) => leaf,
        None => full_name,
    }
}

fn literal_prefix(glob: &str) -> &str {
    match glob.find(['*', '?', '[']) {
        Some(pos) => &glob[..pos],
        None => glob,
    }
}

fn pair_key(full_name: &str, glob: &str, by: PairBy) -> String {
    match by {
        PairBy::Leaf => leaf_of(full_name).to_string(),
        PairBy::Path => full_name
            .strip_prefix(literal_prefix(glob))
            .unwrap_or(full_name)
            .to_string(),
    }
}

#[derive(Debug, Default)]
struct Expanded {
    pairs: Vec<(String, String)>,
    only_a: Vec<String>,
    only_b: Vec<String>,
}

fn matching_vars(h: &Hierarchy, glob: &str) -> Result<Vec<(String, String)>> {
    let pattern =
        glob::Pattern::new(glob).map_err(|e| anyhow::anyhow!("invalid pattern \"{glob}\": {e}"))?;
    let mut out = Vec::new();
    for v in h.all_vars() {
        let full = resolve::var_full_name(h, v);
        if pattern.matches(&full) {
            out.push((pair_key(&full, glob, PairBy::Leaf), full));
        }
    }
    Ok(out)
}

fn expand_pairs(
    h_a: &Hierarchy,
    h_b: &Hierarchy,
    glob_a: &str,
    glob_b: &str,
    by: PairBy,
) -> Result<Expanded> {
    let list_a = matching_vars(h_a, glob_a)?;
    let list_b = matching_vars(h_b, glob_b)?;

    let mut map_a: Vec<(String, String)> = list_a
        .iter()
        .map(|(_, full)| (pair_key(full, glob_a, by), full.clone()))
        .collect();
    let mut map_b: Vec<(String, String)> = list_b
        .iter()
        .map(|(_, full)| (pair_key(full, glob_b, by), full.clone()))
        .collect();
    for (label, list) in [("A", &map_a), ("B", &map_b)] {
        let mut keys: Vec<&String> = list.iter().map(|(k, _)| k).collect();
        keys.sort();
        for pair in keys.windows(2) {
            if pair[0] == pair[1] {
                let dupes: Vec<&str> = list
                    .iter()
                    .filter(|(k, _)| k == pair[0])
                    .map(|(_, f)| f.as_str())
                    .collect();
                bail!(
                    "pairing by {}: \"{}\" is ambiguous on side {} ({}); use --pair-by path",
                    if by == PairBy::Leaf { "leaf" } else { "path" },
                    pair[0],
                    label,
                    dupes.join(", ")
                );
            }
        }
    }
    map_a.sort();
    map_b.sort();

    let mut out = Expanded::default();
    for (key, full_a) in &map_a {
        match map_b.iter().find(|(k, _)| k == key) {
            Some((_, full_b)) => out.pairs.push((full_a.clone(), full_b.clone())),
            None => out.only_a.push(full_a.clone()),
        }
    }
    for (key, full_b) in &map_b {
        if !map_a.iter().any(|(k, _)| k == key) {
            out.only_b.push(full_b.clone());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// shared setup
// ---------------------------------------------------------------------------

/// Load both sides, resolve their clocks, anchors and origins.
///
/// `handles_for_anchor` must already contain the anchor handles (they are
/// resolved before the origins are computed).
fn load_sides<'w>(
    app: &App,
    wf_a: &'w Waveform,
    wf_b: Option<&'w Waveform>,
    clock_a: &'w ClockInfo,
    clock_b: &'w ClockInfo,
    anchor_a: Option<&SigHandle>,
    anchor_b: Option<&SigHandle>,
) -> Result<Ctx<'w>> {
    let path_a = app.require_file()?.clone();
    let view_wf_b = hierarchy_of(wf_a, wf_b);
    let origin_a = resolve_origin(Side::A, wf_a, clock_a, anchor_a, app.cycle_offset)?;
    let shared = wf_b.is_none();
    let origin_b = side_b_origin(
        shared,
        origin_a,
        view_wf_b,
        clock_b,
        anchor_b,
        app.b_cycle_offset,
    )?;
    Ok(Ctx {
        wf_a,
        wf_b: view_wf_b,
        shared,
        clock_a,
        clock_b,
        origin_a,
        origin_b,
        path_b: app.b_file.clone().unwrap_or_else(|| path_a.clone()),
        path_a,
    })
}

/// Resolve both clocks (B reuses A's when there is one file and no --b-clock).
fn resolve_clocks(
    app: &App,
    wf_a: &mut Waveform,
    wf_b: Option<&mut Waveform>,
) -> Result<(ClockInfo, ClockInfo)> {
    let clock_a = resolve_side_clock(
        wf_a,
        app.clock_spec.as_deref(),
        app.edge,
        app.quiet,
        Side::A,
    )?;
    let b_spec = app.b_clock_spec.clone();
    let clock_b = match wf_b {
        Some(wb) => resolve_side_clock(wb, b_spec.as_deref(), app.edge, app.quiet, Side::B)?,
        None => match b_spec.as_deref() {
            Some(spec) => resolve_side_clock(wf_a, Some(spec), app.edge, app.quiet, Side::B)?,
            None => clock_a.clone(),
        },
    };
    Ok((clock_a, clock_b))
}

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

fn delta_or_bound(opts: &Options) -> String {
    match opts.max_delta {
        Some(bound) => format!("|delta| > {bound}"),
        None => "not equal".to_string(),
    }
}

fn origins_fields(ctx: &Ctx<'_>) -> Vec<Field> {
    let mut out = Vec::new();
    if ctx.origin_a != 0 {
        out.push(Field::n("a_cycle_origin", ctx.origin_a));
    }
    if ctx.origin_b != 0 {
        out.push(Field::n("b_cycle_origin", ctx.origin_b));
    }
    out
}

fn side_fields(ctx: &Ctx<'_>, app: &App) -> Vec<Field> {
    let mut out = vec![
        Field::s("file", ctx.path_a.display().to_string()),
        Field::s("clock", ctx.clock_a.full_name.clone()),
    ];
    if !ctx.shared {
        out.push(Field::s("b_file", ctx.path_b.display().to_string()));
        out.push(Field::s("b_clock", ctx.clock_b.full_name.clone()));
    } else if app.b_clock_spec.is_some() {
        out.push(Field::s("b_clock", ctx.clock_b.full_name.clone()));
    }
    out.extend(origins_fields(ctx));
    out
}

// ---------------------------------------------------------------------------
// align
// ---------------------------------------------------------------------------

pub fn align(app: &App, args: AlignArgs) -> Result<Doc> {
    let a_path = app.require_file()?.clone();
    let opts = resolve_options(app, &args.common, None);
    let (lo, hi) = match args.shifts.as_deref() {
        Some(s) => parse_shift_range(s)?,
        None => (0, opts.max_shift),
    };
    if hi < lo {
        bail!("empty shift range {lo}..={hi}");
    }

    let mut wf_a = load::full(&a_path, &load::load_options())?;
    let mut wf_b = match &app.b_file {
        Some(p) => Some(load::full(p, &load::load_options())?),
        None => None,
    };
    let sign;
    let (a, b, gate, streams, anchors) = {
        sign = app.sign_args(wf_a.hierarchy())?;
        let ctx = app.ctx(&sign);
        let a = resolve_on(&wf_a, wf_b.as_ref(), Side::A, &ctx, &args.a, "--a")?;
        let b = resolve_on(&wf_a, wf_b.as_ref(), Side::B, &ctx, &args.b, "--b")?;
        let gate = resolve_flag(
            &wf_a,
            wf_b.as_ref(),
            Side::A,
            &ctx,
            opts.gate.as_ref(),
            "--gate",
        )?;
        let streams = resolve_streams(&wf_a, wf_b.as_ref(), &ctx, &opts)?;
        let anchor_a = resolve_flag(
            &wf_a,
            wf_b.as_ref(),
            Side::A,
            &ctx,
            app.anchor.as_ref(),
            "--anchor",
        )?;
        let anchor_b = resolve_flag(
            &wf_a,
            wf_b.as_ref(),
            Side::B,
            &ctx,
            app.b_anchor.as_ref(),
            "--b-anchor",
        )?;
        (a, b, gate, streams, (anchor_a, anchor_b))
    };
    let (anchor_a, anchor_b) = anchors;

    let (clock_a, clock_b) = resolve_clocks(app, &mut wf_a, wf_b.as_mut())?;

    let mut to_load: Vec<&SigHandle> = vec![&a, &b];
    if let Some(g) = &gate {
        to_load.push(g);
    }
    if let Some(s) = &streams {
        to_load.extend(s.handles());
    }
    if let Some(x) = &anchor_a {
        to_load.push(x);
    }
    if let Some(x) = &anchor_b {
        to_load.push(x);
    }
    load_handles(&mut wf_a, wf_b.as_mut(), &to_load);

    let b_provider = BProvider::Signal(b.clone());
    let ctx = load_sides(
        app,
        &wf_a,
        wf_b.as_ref(),
        &clock_a,
        &clock_b,
        anchor_a.as_ref(),
        anchor_b.as_ref(),
    )?;
    let sides = ctx.sides();
    let ts_a = sides.a.ts();

    let default_to = match opts.align {
        AlignMode::Cycles => sides.a.last_data_cycle(&[&a, &b]),
        AlignMode::Transfers => sides.a.last_logical_cycle(),
    };
    let bounds = resolve_bounds(
        &opts,
        &sides.a,
        ts_a,
        default_to.clamp(0, sides.a.last_logical_cycle()),
    )?;

    let mut columns = vec![
        "shift".to_string(),
        "compared".to_string(),
        "mismatches".to_string(),
        "max_abs_delta".to_string(),
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
            &sides,
            &a,
            &b_provider,
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
                fmt_time(sides.b.ts(), sides.b.tick_of(d.b_cycle).unwrap_or(0)),
                d.index.to_string(),
            ),
            None => ("-".to_string(), "-".to_string(), "-".to_string()),
        };
        let delta_text = scan
            .max_abs_delta
            .map(|d| d.to_text())
            .unwrap_or_else(|| "-".to_string());
        let mut text = vec![
            shift.to_string(),
            scan.compared.to_string(),
            scan.mismatches.to_string(),
            delta_text,
            first_cycle,
            first_time.clone(),
        ];
        let mut row = json!({
            "shift": shift,
            "compared": scan.compared,
            "mismatches": scan.mismatches,
            "x_involved": scan.x_involved,
            "skipped": scan.skipped,
            "max_abs_delta": scan.max_abs_delta.map(|d| d.to_json()),
            "max_bits_differ": scan.max_bits_differ,
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

    let mut doc = Doc::new();
    for f in side_fields(&ctx, app) {
        doc = doc.field(f);
    }
    doc = doc
        .field(Field::s("edge", app.edge.name()))
        .field(Field::s("align", opts.align.name()))
        .field(Field::s("cmp", opts.cmp_name()))
        .field(Field::s("x", opts.x.name()));
    if let Some(bound) = opts.max_delta {
        doc = doc.field(Field::n(
            "max_delta",
            i64::try_from(bound).unwrap_or(i64::MAX),
        ));
    }
    doc = doc
        .field(Field::n("from_cycle", bounds.from))
        .field(Field::n("to_cycle", bounds.to))
        .field(Field::s("a", a.full_name.clone()))
        .field(Field::s("b", b_provider.label()))
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
            .trailer_field(Field::u("x_involved", scan.x_involved))
            .trailer_field(Field::s("divergence_rule", delta_or_bound(&opts)));
        if let Some(delta) = scan.max_abs_delta {
            doc = doc.trailer_field(Field::text_value(
                "max_abs_delta",
                delta.to_text(),
                delta.to_json(),
            ));
        }
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// stream setup
// ---------------------------------------------------------------------------

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
    wf_a: &Waveform,
    wf_b: Option<&Waveform>,
    ctx: &resolve::ResolveCtx<'_>,
    opts: &Options,
) -> Result<Option<Streams>> {
    if opts.align != AlignMode::Transfers {
        return Ok(None);
    }
    if opts.gate.is_some() {
        bail!("--gate is not meaningful with --align transfers");
    }
    let in_valid = resolve_flag(
        wf_a,
        wf_b,
        Side::A,
        ctx,
        opts.in_valid.as_ref(),
        "--in-valid",
    )?
    .ok_or_else(|| anyhow::anyhow!("--align transfers requires --in-valid <NAME>"))?;
    let out_valid = resolve_flag(
        wf_a,
        wf_b,
        Side::B,
        ctx,
        opts.out_valid.as_ref(),
        "--out-valid",
    )?
    .ok_or_else(|| anyhow::anyhow!("--align transfers requires --out-valid <NAME>"))?;
    Ok(Some(Streams {
        in_valid,
        in_ready: resolve_flag(
            wf_a,
            wf_b,
            Side::A,
            ctx,
            opts.in_ready.as_ref(),
            "--in-ready",
        )?,
        out_valid,
        out_ready: resolve_flag(
            wf_a,
            wf_b,
            Side::B,
            ctx,
            opts.out_ready.as_ref(),
            "--out-ready",
        )?,
    }))
}

#[allow(clippy::too_many_arguments)]
fn run_scan(
    sides: &Sides<'_>,
    a: &SigHandle,
    b: &BProvider,
    gate: Option<&SigHandle>,
    streams: Option<&Streams>,
    bounds: Bounds,
    shift: u32,
    opts: &Options,
    collect: bool,
) -> Scan {
    match opts.align {
        AlignMode::Cycles => scan_cycles(sides, a, b, gate, bounds, shift, opts, collect),
        AlignMode::Transfers => {
            let streams = streams.expect("transfers mode requires event streams");
            scan_transfers(
                sides,
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
// compare
// ---------------------------------------------------------------------------

fn parse_map(s: &str) -> Result<(String, String, Option<u32>)> {
    let (a, rest) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--map/--map-glob expects A=B or A=B:SHIFT, got \"{s}\""))?;
    let a = a.trim();
    if a.is_empty() {
        bail!("--map/--map-glob expects A=B or A=B:SHIFT, got \"{s}\"");
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

fn looks_like_glob(spec: &str) -> bool {
    spec.contains('*') || spec.contains('?')
}

struct PairResult {
    scan: Scan,
    shift: u32,
    ambiguous: Option<bool>,
    auto_mismatches: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
pub fn compare(app: &App, args: CompareArgs) -> Result<Doc> {
    let a_path = app.require_file()?.clone();
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
    let pair_by = PairBy::parse(
        args.pair_by
            .as_deref()
            .or(named.as_ref().and_then(|c| c.pair_by.as_deref())),
    )?;

    // explicit pairs: the named check's pair, then CLI --map entries
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

    let mut wf_a = load::full(&a_path, &load::load_options())?;
    let mut wf_b = match &app.b_file {
        Some(p) => Some(load::full(p, &load::load_options())?),
        None => None,
    };
    // glob pair sources: config [compare.pairs], --map-glob, --pair-scopes
    let mut glob_pairs: Vec<(String, String)> = Vec::new();
    if let Some(cfg_pairs) = app.cfg.compare.as_ref().and_then(|c| c.pairs.as_ref()) {
        for (a, b) in cfg_pairs {
            glob_pairs.push((a.clone(), b.clone()));
        }
    }
    if let Some(named) = &named
        && let Some(cfg_pairs) = named.pairs.as_ref()
    {
        for (a, b) in cfg_pairs {
            glob_pairs.push((a.clone(), b.clone()));
        }
    }
    if let Some(scopes) = args
        .pair_scopes
        .as_deref()
        .or(named.as_ref().and_then(|c| c.pair_scopes.as_deref()))
    {
        let (sa, sb) = scopes.split_once(',').ok_or_else(|| {
            anyhow::anyhow!("--pair-scopes expects two scope prefixes: A,B (got \"{scopes}\")")
        })?;
        glob_pairs.push((format!("{}.*", sa.trim()), format!("{}.*", sb.trim())));
    }
    for spec in args
        .map_glob
        .iter()
        .chain(named.iter().flat_map(|c| c.map_glob.iter().flatten()))
    {
        let (a, b, shift) = parse_map(spec)?;
        if shift.is_some() {
            bail!("--map-glob does not take a shift (\"{spec}\"); use --shift or --auto-shift");
        }
        glob_pairs.push((a, b));
    }

    let mut only_a: Vec<String> = Vec::new();
    let mut only_b: Vec<String> = Vec::new();
    {
        let h_a = wf_a.hierarchy();
        let h_b = hierarchy_of(&wf_a, wf_b.as_ref()).hierarchy();
        for (glob_a, glob_b) in &glob_pairs {
            let expanded = expand_pairs(h_a, h_b, glob_a, glob_b, pair_by)?;
            only_a.extend(expanded.only_a);
            only_b.extend(expanded.only_b);
            for (a, b) in expanded.pairs {
                pairs.push((a, b, None));
            }
        }
    }

    if pairs.is_empty() {
        bail!(
            "no comparison pairs: pass --map A=B[:SHIFT] (repeatable), --map-glob 'A.*=B.*', \
             --pair-scopes A,B, or --check NAME"
        );
    }
    if let Some(shift) = args.shift {
        for pair in &mut pairs {
            pair.2 = Some(shift);
        }
    }

    for name in &only_a {
        app.note(&format!(
            "compare: no side B signal pairs with \"{name}\" (nothing to compare for it)"
        ));
    }
    for name in &only_b {
        app.note(&format!(
            "compare: no side A signal pairs with \"{name}\" (nothing to compare for it)"
        ));
    }

    let sign;
    let probe_handles = {
        let h = wf_a.hierarchy();
        sign = app.sign_args(h)?;
        if args.probe.is_empty() {
            Vec::new()
        } else {
            let specs = app.expand_signals(&args.probe)?;
            let ctx = app.ctx(&sign);
            specs
                .iter()
                .map(|s| resolve_on(&wf_a, wf_b.as_ref(), Side::A, &ctx, s, "--probe"))
                .collect::<Result<Vec<_>>>()?
        }
    };

    let (gate, streams, anchors) = {
        let ctx = app.ctx(&sign);
        let gate = resolve_flag(
            &wf_a,
            wf_b.as_ref(),
            Side::A,
            &ctx,
            opts.gate.as_ref(),
            "--gate",
        )?;
        let streams = resolve_streams(&wf_a, wf_b.as_ref(), &ctx, &opts)?;
        let anchor_a = resolve_flag(
            &wf_a,
            wf_b.as_ref(),
            Side::A,
            &ctx,
            app.anchor.as_ref(),
            "--anchor",
        )?;
        let anchor_b = resolve_flag(
            &wf_a,
            wf_b.as_ref(),
            Side::B,
            &ctx,
            app.b_anchor.as_ref(),
            "--b-anchor",
        )?;
        (gate, streams, (anchor_a, anchor_b))
    };
    let (anchor_a, anchor_b) = anchors;

    let mut expect_cache = ExpectCache::default();
    let resolved_pairs: Vec<(SigHandle, BProvider, Option<u32>)> = {
        let ctx = app.ctx(&sign);
        let mut out = Vec::with_capacity(pairs.len());
        for (a_spec, b_spec, shift) in &pairs {
            let a_handle = if looks_like_glob(a_spec) {
                bail!(
                    "side A spec \"{a_spec}\" looks like a glob: use --map-glob '{a_spec}={b_spec}' \
                     or --pair-scopes to pair signals by name"
                );
            } else {
                resolve_on(
                    &wf_a,
                    wf_b.as_ref(),
                    Side::A,
                    &ctx,
                    a_spec,
                    "--map (side A)",
                )?
            };
            let provider = if crate::expect::is_expect(b_spec) {
                let spec = if b_spec == "@" || b_spec.starts_with("@:") {
                    let path = app.expect_file.clone().ok_or_else(|| {
                        anyhow::anyhow!(
                            "{b_spec} needs --expect-file <PATH> (or a full @FILE:cycle,value)"
                        )
                    })?;
                    crate::expect::parse_spec(&format!("@{}{}", path.display(), &b_spec[1..]))?
                } else {
                    crate::expect::parse_spec(b_spec)?
                };
                if wf_b.is_some() {
                    app.note(&format!(
                        "compare: {b_spec} is an expected-value file; --b-file is ignored for this pair"
                    ));
                }
                let table = expect_cache.load(&spec)?;
                // the expected side borrows width/radix/signedness from side A
                let mut meta = a_handle.clone().on_side(Side::B);
                meta.full_name = spec.label();
                meta.name = spec.label();
                meta.var = VarRef::default();
                meta.signal_ref = SignalRef::from_index(0).expect("signal ref");
                meta.index = None;
                meta.slice = None;
                meta.encoding = SignalEncoding::BitVector(meta.eff_width());
                meta.var_type = "expect";
                // validate every value up front: a golden value that does not fit
                // must never be silently skipped
                for cycle in table.cycles() {
                    table.value(cycle, meta.eff_width(), meta.signed)?;
                }
                BProvider::Expect {
                    table,
                    meta,
                    spec: spec.label(),
                }
            } else {
                BProvider::Signal(resolve_on(
                    &wf_a,
                    wf_b.as_ref(),
                    Side::B,
                    &ctx,
                    b_spec,
                    "--map (side B)",
                )?)
            };
            out.push((a_handle, provider, *shift));
        }
        out
    };

    let (clock_a, clock_b) = resolve_clocks(app, &mut wf_a, wf_b.as_mut())?;

    let mut to_load: Vec<&SigHandle> = Vec::new();
    for (a, b, _) in &resolved_pairs {
        to_load.push(a);
        if let Some(handle) = b.signal() {
            to_load.push(handle);
        }
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
    if let Some(x) = &anchor_a {
        to_load.push(x);
    }
    if let Some(x) = &anchor_b {
        to_load.push(x);
    }
    load_handles(&mut wf_a, wf_b.as_mut(), &to_load);

    let ctx = load_sides(
        app,
        &wf_a,
        wf_b.as_ref(),
        &clock_a,
        &clock_b,
        anchor_a.as_ref(),
        anchor_b.as_ref(),
    )?;
    let sides = ctx.sides();
    let ts_a = sides.a.ts();
    let ts_b = sides.b.ts();

    let all_a: Vec<&SigHandle> = resolved_pairs.iter().map(|(a, _, _)| a).collect();
    let default_to = match opts.align {
        AlignMode::Cycles => sides.a.last_data_cycle(&all_a),
        AlignMode::Transfers => sides.a.last_logical_cycle(),
    };
    let bounds = resolve_bounds(
        &opts,
        &sides.a,
        ts_a,
        default_to.clamp(0, sides.a.last_logical_cycle()),
    )?;

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
                    &sides,
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
            "pair {}={}: specify a shift (A=B:3 or --shift 0) or pass --auto-shift",
            a.full_name,
            b.label()
        );
    }

    // final scan per pair (with divergence rows)
    let mut results: Vec<PairResult> = Vec::new();
    for (i, (a, b, _)) in resolved_pairs.iter().enumerate() {
        let scan = run_scan(
            &sides,
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
            let a_edge = sides.a.tick_of(r.div.a_cycle);
            let b_edge = sides.b.tick_of(r.div.b_cycle);
            r.div.a_cell.change_tick == a_edge && r.div.b_cell.change_tick == b_edge
        });
    }
    if args.first_only {
        rows.truncate(1);
    }

    let probe_at = args
        .probe_at
        .or(named.as_ref().and_then(|c| c.probe_at).map(|p| match p {
            crate::config::ProbeAtCfg::ACycle => ProbeAt::ACycle,
            crate::config::ProbeAtCfg::BCycle => ProbeAt::BCycle,
        }))
        .unwrap_or(ProbeAt::ACycle);
    let show_delta = args.delta || named.as_ref().and_then(|c| c.delta).unwrap_or(false);

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
    ];
    if show_delta {
        columns.push("delta".to_string());
        columns.push("bits_differ".to_string());
    }
    columns.push("cmp".to_string());
    columns.push("a_at".to_string());
    columns.push("b_at".to_string());
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
        let (a_handle, b_provider, _) = &resolved_pairs[row.pair];
        let b_handle = b_provider.handle();
        let b_tick = sides.b.tick_of(row.div.b_cycle);
        let a_tick = sides.a.tick_of(row.div.a_cycle);
        let time = fmt_time(ts_b, b_tick.unwrap_or(0));
        let a_at = row
            .div
            .a_cell
            .change_tick
            .map(|t| format_time(t, ts_a))
            .unwrap_or_else(|| "-".to_string());
        let b_at = row
            .div
            .b_cell
            .change_tick
            .map(|t| format_time(t, ts_b))
            .unwrap_or_else(|| "-".to_string());
        let mut text = vec![
            row.div.index.to_string(),
            time.clone(),
            row.div.a_cycle.to_string(),
            row.div.b_cycle.to_string(),
            row.pair.to_string(),
            row.div.a_cell.text(a_handle.radix, a_handle.signed),
            row.div.b_cell.text(b_handle.radix, b_handle.signed),
        ];
        if show_delta {
            text.push(
                row.div
                    .verdict
                    .delta
                    .map(|d| d.to_text())
                    .unwrap_or_else(|| "-".to_string()),
            );
            text.push(
                row.div
                    .verdict
                    .bits_differ
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            );
        }
        text.push(row.div.verdict.label.to_string());
        text.push(a_at);
        text.push(b_at);
        let probe_cycle = match probe_at {
            ProbeAt::ACycle => row.div.a_cycle,
            ProbeAt::BCycle => row.div.b_cycle,
        };
        let mut probes = Map::new();
        for p in &probe_handles {
            let view = sides.view(p.side);
            let cell = view.cell(p, probe_cycle);
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
        json_row.insert(
            "b_cycle_local".into(),
            sides.b.local_of(row.div.b_cycle).into(),
        );
        json_row.insert("time".into(), time.into());
        if let Some(t) = a_tick {
            json_row.insert("a_time".into(), format_time(t, ts_a).into());
        }
        if let Some(t) = b_tick {
            json_row.insert("b_time".into(), format_time(t, ts_b).into());
        }
        json_row.insert("pair".into(), row.pair.into());
        json_row.insert("a_signal".into(), a_handle.full_name.clone().into());
        json_row.insert("b_signal".into(), b_provider.label().into());
        json_row.insert(
            "a".into(),
            Value::Object(value_json(&row.div.a_cell, a_handle)),
        );
        json_row.insert(
            "b".into(),
            Value::Object(value_json(&row.div.b_cell, b_handle)),
        );
        if show_delta {
            json_row.insert(
                "delta".into(),
                row.div
                    .verdict
                    .delta
                    .map(|d| d.to_json())
                    .unwrap_or(Value::Null),
            );
            json_row.insert(
                "bits_differ".into(),
                row.div
                    .verdict
                    .bits_differ
                    .clone()
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            );
        }
        json_row.insert("diff_bits".into(), row.div.verdict.diff_bits.into());
        json_row.insert("cmp".into(), row.div.verdict.label.into());
        json_row.insert(
            "a_change_time".into(),
            row.div
                .a_cell
                .change_tick
                .map(|t| Value::from(format_time(t, ts_a)))
                .unwrap_or(Value::Null),
        );
        json_row.insert(
            "b_change_time".into(),
            row.div
                .b_cell
                .change_tick
                .map(|t| Value::from(format_time(t, ts_b)))
                .unwrap_or(Value::Null),
        );
        json_row.insert("pre_clock".into(), (row.div.b_cycle < 0).into());
        json_row.insert("a_side".into(), a_handle.side.name().into());
        json_row.insert("b_side".into(), b_handle.side.name().into());
        if !probes.is_empty() {
            json_row.insert("probe".into(), Value::Object(probes));
            json_row.insert(
                "probe_at".into(),
                match probe_at {
                    ProbeAt::ACycle => "a_cycle".into(),
                    ProbeAt::BCycle => "b_cycle".into(),
                },
            );
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
        "max_abs_delta".to_string(),
        "max_bits_differ".to_string(),
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
        let (a_handle, b_provider, _) = &resolved_pairs[i];
        let first_time = r
            .scan
            .first_div
            .as_ref()
            .and_then(|d| sides.b.tick_of(d.b_cycle))
            .map(|t| format_time(t, ts_b));
        let delta_text = r
            .scan
            .max_abs_delta
            .map(|d| d.to_text())
            .unwrap_or_else(|| "-".to_string());
        let mut text = vec![
            i.to_string(),
            a_handle.full_name.clone(),
            b_provider.label(),
            r.shift.to_string(),
            opts.align.name().to_string(),
            r.scan.compared.to_string(),
            r.scan.mismatches.to_string(),
            r.scan.x_involved.to_string(),
            r.scan.skipped.to_string(),
            delta_text,
            r.scan.max_bits_differ.to_string(),
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
            "b": b_provider.label(),
            "shift": r.shift,
            "align": opts.align.name(),
            "cmp": opts.cmp_name(),
            "x": opts.x.name(),
            "compared": r.scan.compared,
            "mismatches": r.scan.mismatches,
            "x_involved": r.scan.x_involved,
            "skipped": r.scan.skipped,
            "max_abs_delta": r.scan.max_abs_delta.map(|d| d.to_json()),
            "max_bits_differ": r.scan.max_bits_differ,
            "delta_units": r.scan.delta_units,
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

    for (i, r) in results.iter().enumerate() {
        let (a_handle, b_provider, _) = &resolved_pairs[i];
        if matches!(b_provider, BProvider::Expect { .. }) && r.scan.skipped > 0 {
            app.note(&format!(
                "compare: pair {}={}: {} cycle(s) are not covered by the expected-value file and were skipped",
                a_handle.full_name,
                b_provider.label(),
                r.scan.skipped
            ));
        }
    }
    if opts.align == AlignMode::Transfers {
        for (i, r) in results.iter().enumerate() {
            if r.scan.unpaired > 0 {
                let (a_handle, b_provider, _) = &resolved_pairs[i];
                app.note(&format!(
                    "compare: transfers mode: pair {}={}: a_events={} b_events={} unpaired={} (expected to equal \
                     the pipeline depth; a difference between runs of the same design indicates swallowed or duplicated transfers)",
                    a_handle.full_name, b_provider.label(), r.scan.a_events, r.scan.b_events, r.scan.unpaired
                ));
            }
        }
    }

    let mut doc = Doc::new();
    for f in side_fields(&ctx, app) {
        doc = doc.field(f);
    }
    doc = doc
        .field(Field::s("edge", app.edge.name()))
        .field(Field::s("align", opts.align.name()))
        .field(Field::s("cmp", opts.cmp_name()))
        .field(Field::s("x", opts.x.name()));
    if let Some(bound) = opts.max_delta {
        doc = doc.field(Field::n(
            "max_delta",
            i64::try_from(bound).unwrap_or(i64::MAX),
        ));
    }
    doc = doc
        .field(Field::n("from_cycle", bounds.from))
        .field(Field::n("to_cycle", bounds.to))
        .field(Field::u("pair_count", results.len() as u64))
        .field(Field::s("divergence_rule", delta_or_bound(&opts)));
    if !only_a.is_empty() {
        doc = doc
            .field(Field::u("unpaired_a_signals", only_a.len() as u64))
            .field(Field::text_value(
                "unpaired_a",
                only_a.join(", "),
                json!(only_a),
            ));
    }
    if !only_b.is_empty() {
        doc = doc
            .field(Field::u("unpaired_b_signals", only_b.len() as u64))
            .field(Field::text_value(
                "unpaired_b",
                only_b.join(", "),
                json!(only_b),
            ));
    }
    doc = doc
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
    value_map(cell, handle)
}

fn value_map(cell: &Cell, handle: &SigHandle) -> Map<String, Value> {
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
        assert_eq!(Verdict::plain("ok", false, false).label, "ok");
        assert_eq!(Verdict::plain("mismatch", true, false).label, "mismatch");
        assert_eq!(Verdict::plain("x-involved", true, true).label, "x-involved");
    }

    #[test]
    fn numeric_extension() {
        assert!(!numeric_eq(
            &Val::Bits("11111101".into()),
            false,
            8,
            &Val::Bits("11111101".into()),
            true,
            8
        ));
        assert!(numeric_eq(
            &Val::Bits("000111000".into()),
            false,
            9,
            &Val::Bits("00111000".into()),
            false,
            8
        ));
        assert!(numeric_eq(
            &Val::Bits("11111101".into()),
            true,
            8,
            &Val::Bits("111111101".into()),
            true,
            9
        ));
    }

    #[test]
    fn delta_and_mask() {
        let a = SigHandle::test_stub("a", 8, false);
        let b = SigHandle::test_stub("b", 8, false);
        let (delta, mask, bits) = numeric_detail(
            &Val::Bits("00000100".into()),
            &a,
            &Val::Bits("00000110".into()),
            &b,
        );
        assert_eq!(delta, Some(DeltaVal::Int(2)));
        assert_eq!(mask.as_deref(), Some("02"));
        assert_eq!(bits, 1);

        // signed sides are interpreted with their own signedness
        let sa = SigHandle::test_stub("a", 8, true);
        let (delta, _, _) = numeric_detail(
            &Val::Bits("11111111".into()),
            &sa,
            &Val::Bits("00000001".into()),
            &b,
        );
        assert_eq!(delta, Some(DeltaVal::Int(2)));
    }

    #[test]
    fn delta_bounds() {
        assert!(DeltaVal::Int(3).exceeds(2));
        assert!(!DeltaVal::Int(-2).exceeds(2));
        assert!(DeltaVal::Real(-2.5).exceeds(2));
    }

    #[test]
    fn pair_keys() {
        assert_eq!(pair_key("dut.lane0.a0", "dut.*", PairBy::Leaf), "a0");
        assert_eq!(pair_key("dut.lane0.a0", "dut.*", PairBy::Path), "lane0.a0");
        assert_eq!(literal_prefix("dut.*"), "dut.");
    }
}
