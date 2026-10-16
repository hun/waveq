//! Clock resolution: candidate detection, edge extraction and the cycle map.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::io::Write;
use wellen::simple::Waveform;
use wellen::{Hierarchy, Signal, SignalRef, Timescale, VarRef};

use crate::format::{Edge, format_time, timescale_text};
use crate::resolve::{find_var, parse_sigspec};
use crate::sample::sample_at_tick;

/// Collapse consecutive same-tick values (glitches) to the last one.
fn collapsed_values(wf: &Waveform, sig: &Signal) -> Vec<(u64, String)> {
    let tt = wf.time_table();
    let mut out: Vec<(u64, String)> = Vec::new();
    for (idx, value) in sig.iter_changes() {
        let Some(tick) = tt.get(idx as usize).copied() else {
            continue;
        };
        let Some(bits) = value.to_bit_string() else {
            continue;
        };
        match out.last_mut() {
            Some((t, prev)) if *t == tick => *prev = bits,
            _ => out.push((tick, bits)),
        }
    }
    out
}

fn is_rising(prev: &str, cur: &str) -> bool {
    !prev.ends_with('1') && cur.ends_with('1')
}

fn is_falling(prev: &str, cur: &str) -> bool {
    !prev.ends_with('0') && cur.ends_with('0')
}

/// Ticks of the selected edges, strictly increasing.
pub fn extract_edges(wf: &Waveform, sig: &Signal, edge: Edge) -> Vec<u64> {
    let values = collapsed_values(wf, sig);
    let mut out = Vec::new();
    for pair in values.windows(2) {
        let (t_prev, prev) = &pair[0];
        let (t_cur, cur) = &pair[1];
        if t_prev >= t_cur {
            continue;
        }
        let keep = match edge {
            Edge::Posedge => is_rising(prev, cur),
            Edge::Negedge => is_falling(prev, cur),
            Edge::Both => is_rising(prev, cur) || is_falling(prev, cur),
        };
        if keep {
            out.push(*t_cur);
        }
    }
    out
}

/// Cycle index of a tick: -1 when the tick is before the first edge.
pub fn cycle_of(edges: &[u64], tick: u64) -> i64 {
    edges.partition_point(|e| *e <= tick) as i64 - 1
}

fn periods(edges: &[u64]) -> (Option<u64>, Option<u64>, Option<u64>) {
    if edges.len() < 2 {
        return (None, None, None);
    }
    let mut deltas: Vec<u64> = edges.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_unstable();
    (
        deltas.first().copied(),
        deltas.last().copied(),
        deltas.get(deltas.len() / 2).copied(),
    )
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub full_name: String,
    pub var: VarRef,
    pub edges: usize,
    pub median_period: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectBy {
    Cli,
    Name,
    Fallback,
}

impl DetectBy {
    pub fn name(self) -> &'static str {
        match self {
            DetectBy::Cli => "cli",
            DetectBy::Name => "name",
            DetectBy::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClockInfo {
    pub var: VarRef,
    pub full_name: String,
    pub edge: Edge,
    pub edges: Vec<u64>,
    pub period_min: Option<u64>,
    pub period_max: Option<u64>,
    pub period_median: Option<u64>,
    pub candidates: Vec<Candidate>,
    pub detected_by: DetectBy,
}

impl ClockInfo {
    pub fn regular(&self) -> bool {
        match (self.period_min, self.period_max) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    pub fn cycle_of(&self, tick: u64) -> i64 {
        cycle_of(&self.edges, tick)
    }

    pub fn tick_of(&self, cycle: i64) -> Option<u64> {
        self.edges.get(usize::try_from(cycle).ok()?).copied()
    }

    pub fn last_cycle(&self) -> i64 {
        self.edges.len() as i64 - 1
    }

    pub fn summary_json(&self, ts: Option<Timescale>) -> Value {
        json!({
            "signal": self.full_name,
            "edge": self.edge.name(),
            "edges": self.edges.len(),
            "first_tick": self.edges.first().copied(),
            "first_time": self.edges.first().map(|t| format_time(*t, ts)),
            "last_tick": self.edges.last().copied(),
            "last_time": self.edges.last().map(|t| format_time(*t, ts)),
            "period_min": self.period_min,
            "period_max": self.period_max,
            "period_median": self.period_median,
            "regular": self.regular(),
            "detected_by": self.detected_by.name(),
        })
    }

    pub fn summary_text(&self, ts: Option<Timescale>) -> String {
        let first = self
            .edges
            .first()
            .map(|t| format!("{} (tick {})", format_time(*t, ts), t))
            .unwrap_or_else(|| "-".to_string());
        let last = self
            .edges
            .last()
            .map(|t| format!("{} (tick {})", format_time(*t, ts), t))
            .unwrap_or_else(|| "-".to_string());
        let period = match self.period_median {
            Some(p) => format!(
                "{} (tick {}){}{}",
                format_time(p, ts),
                p,
                if self.regular() { ", regular" } else { "" },
                match (self.period_min, self.period_max) {
                    (Some(a), Some(b)) if a != b => {
                        format!(", min {} max {}", format_time(a, ts), format_time(b, ts))
                    }
                    _ => String::new(),
                }
            ),
            None => "-".to_string(),
        };
        format!(
            "{} ({} edge, {} edges, {}..{}, period {}, detected by {})",
            self.full_name,
            self.edge.name(),
            self.edges.len(),
            first,
            last,
            period,
            self.detected_by.name()
        )
    }
}

fn last_component(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, last)) => last,
        None => name,
    }
}

fn looks_like_clock(h: &Hierarchy, v: VarRef) -> bool {
    let full = h[v].full_name(h);
    let last = last_component(&full).to_lowercase();
    last.contains("clk") || last.contains("clock")
}

fn candidate_for(wf: &Waveform, h: &Hierarchy, v: VarRef, edge: Edge) -> Candidate {
    let edges = match wf.get_signal(h[v].signal_ref()) {
        Some(sig) => extract_edges(wf, sig, edge),
        None => Vec::new(),
    };
    Candidate {
        full_name: h[v].full_name(h),
        var: v,
        edges: edges.len(),
        median_period: periods(&edges).2,
    }
}

fn rank(mut cands: Vec<Candidate>) -> Vec<Candidate> {
    cands.sort_by(|a, b| {
        b.edges
            .cmp(&a.edges)
            .then_with(|| a.var.index().cmp(&b.var.index()))
    });
    cands
}

fn note(quiet: bool, msg: &str) {
    if !quiet {
        let _ = writeln!(std::io::stderr(), "{msg}");
    }
}

fn load_vars(wf: &mut Waveform, vars: &[VarRef]) -> Vec<SignalRef> {
    let ids: Vec<SignalRef> = {
        let h = wf.hierarchy();
        vars.iter().map(|v| h[*v].signal_ref()).collect()
    };
    if !ids.is_empty() {
        wf.load_signals_multi_threaded(&ids);
    }
    ids
}

fn edges_of(wf: &Waveform, h: &Hierarchy, var: VarRef, edge: Edge) -> Vec<u64> {
    match wf.get_signal(h[var].signal_ref()) {
        Some(sig) => extract_edges(wf, sig, edge),
        None => Vec::new(),
    }
}

fn build(
    h: &Hierarchy,
    var: VarRef,
    edge: Edge,
    edges: Vec<u64>,
    candidates: Vec<Candidate>,
    detected_by: DetectBy,
) -> ClockInfo {
    let (min, max, median) = periods(&edges);
    ClockInfo {
        var,
        full_name: h[var].full_name(h),
        edge,
        edges,
        period_min: min,
        period_max: max,
        period_median: median,
        candidates,
        detected_by,
    }
}

/// Resolve the clock: explicit name, `none`, or auto-detection.
pub fn resolve_clock(
    wf: &mut Waveform,
    spec: Option<&str>,
    edge: Edge,
    quiet: bool,
) -> Result<Option<ClockInfo>> {
    if let Some(name) = spec
        && name.eq_ignore_ascii_case("none")
    {
        return Ok(None);
    }

    if let Some(name) = spec {
        let sig_spec = parse_sigspec(name)?;
        let var = {
            let h = wf.hierarchy();
            find_var(h, &sig_spec)
                .map_err(|e| anyhow::anyhow!("--clock {name}: {e}"))?
                .0
        };
        load_vars(wf, &[var]);
        let h = wf.hierarchy();
        let edges = edges_of(wf, h, var, edge);
        if edges.is_empty() {
            let ticks = wf.time_table().len();
            bail!(
                "clock \"{}\": no {} edges found ({} ticks, timescale {})",
                h[var].full_name(h),
                edge.name(),
                ticks,
                timescale_text(h.timescale())
            );
        }
        return Ok(Some(build(h, var, edge, edges, Vec::new(), DetectBy::Cli)));
    }

    // Phase 1: 1-bit signals whose leaf name mentions clk/clock.
    let named: Vec<VarRef> = {
        let h = wf.hierarchy();
        h.all_vars()
            .filter(|v| h[*v].is_1bit(h) && looks_like_clock(h, *v))
            .collect()
    };
    load_vars(wf, &named);
    let named_cands = {
        let h = wf.hierarchy();
        rank(
            named
                .iter()
                .map(|v| candidate_for(wf, h, *v, edge))
                .collect(),
        )
    };
    if let Some(best) = named_cands.iter().find(|c| c.edges >= 4).cloned() {
        let h = wf.hierarchy();
        let edges = edges_of(wf, h, best.var, edge);
        return Ok(Some(build(
            h,
            best.var,
            edge,
            edges,
            named_cands,
            DetectBy::Name,
        )));
    }

    // Phase 2: any 1-bit signal with enough edges.
    let bits: Vec<VarRef> = {
        let h = wf.hierarchy();
        h.all_vars().filter(|v| h[*v].is_1bit(h)).collect()
    };
    load_vars(wf, &bits);
    let all_cands = {
        let h = wf.hierarchy();
        rank(
            bits.iter()
                .map(|v| candidate_for(wf, h, *v, edge))
                .collect(),
        )
    };
    let listing = |cands: &[Candidate]| {
        cands
            .iter()
            .take(8)
            .map(|c| format!("{}({})", c.full_name, c.edges))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(best) = all_cands.iter().find(|c| c.edges >= 8).cloned() {
        note(
            quiet,
            &format!(
                "clock: no name match; using {} ({} edges). candidates: {}",
                best.full_name,
                best.edges,
                listing(&all_cands)
            ),
        );
        let h = wf.hierarchy();
        let edges = edges_of(wf, h, best.var, edge);
        return Ok(Some(build(
            h,
            best.var,
            edge,
            edges,
            all_cands,
            DetectBy::Fallback,
        )));
    }

    if all_cands.is_empty() && named_cands.is_empty() {
        bail!("no clock found. the file has no 1-bit signals; pass --clock <NAME>");
    }
    bail!(
        "no clock found. 1-bit signals by edge count: {}; pass --clock <NAME>",
        listing(&all_cands)
    )
}

/// Value of a 1-bit signal at a tick, for valid/gate checks.
pub fn is_high(wf: &Waveform, h: &Hierarchy, var: VarRef, tick: u64) -> bool {
    match wf.get_signal(h[var].signal_ref()) {
        Some(sig) => match sample_at_tick(wf, sig, tick) {
            Some(s) => s.bits.as_deref() == Some("1"),
            None => false,
        },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collapse(values: &[(u64, &str)]) -> Vec<(u64, String)> {
        let mut collapsed: Vec<(u64, String)> = Vec::new();
        for (t, v) in values {
            match collapsed.last_mut() {
                Some((lt, lv)) if *lt == *t => *lv = (*v).to_string(),
                _ => collapsed.push((*t, (*v).to_string())),
            }
        }
        collapsed
    }

    #[test]
    fn edge_extraction_collapses_glitches() {
        // same-tick pair (15, "0") then (15, "1") collapses to "1": no phantom falling edge
        let collapsed = collapse(&[(0, "0"), (5, "1"), (15, "0"), (15, "1"), (25, "0")]);
        assert_eq!(collapsed.len(), 4);
        let mut rising = Vec::new();
        let mut falling = Vec::new();
        for w in collapsed.windows(2) {
            if is_rising(&w[0].1, &w[1].1) {
                rising.push(w[1].0);
            }
            if is_falling(&w[0].1, &w[1].1) {
                falling.push(w[1].0);
            }
        }
        assert_eq!(rising, vec![5]);
        assert_eq!(falling, vec![25]);
    }

    #[test]
    fn glitch_into_x_is_not_an_edge() {
        let collapsed = collapse(&[(0, "0"), (5, "x"), (15, "1")]);
        let mut rising = Vec::new();
        for w in collapsed.windows(2) {
            if is_rising(&w[0].1, &w[1].1) {
                rising.push(w[1].0);
            }
        }
        assert_eq!(rising, vec![15]);
    }

    #[test]
    fn cycle_of_boundaries() {
        let edges = vec![5u64, 15, 25, 35];
        assert_eq!(cycle_of(&edges, 0), -1);
        assert_eq!(cycle_of(&edges, 4), -1);
        assert_eq!(cycle_of(&edges, 5), 0);
        assert_eq!(cycle_of(&edges, 14), 0);
        assert_eq!(cycle_of(&edges, 15), 1);
        assert_eq!(cycle_of(&edges, 1000), 3);
        assert_eq!(cycle_of(&[], 100), -1);
    }

    #[test]
    fn period_stats() {
        let (min, max, median) = periods(&[5, 15, 25, 35]);
        assert_eq!((min, max, median), (Some(10), Some(10), Some(10)));
        let (min, max, median) = periods(&[5, 15, 30]);
        assert_eq!((min, max, median), (Some(10), Some(15), Some(15)));
        assert_eq!(periods(&[5]).0, None);
    }
}
