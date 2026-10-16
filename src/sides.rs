//! Sides: the A and B sampling sources.
//!
//! Side A is the primary waveform (`-f`), side B is either the same waveform
//! (one-dump comparisons) or a second waveform (`--b-file`, gap "compare side A
//! in one file against side B in another"). Each side has its own clock, its own
//! **origin** (the local edge index that represents *logical* cycle 0), and its
//! own tick base.
//!
//! `origin` folds together two user-facing knobs:
//! * `--anchor NAME` / `--b-anchor NAME`: logical cycle 0 is the first local
//!   cycle where `NAME` is `1` (finite reset lengths differ between runs);
//! * `--b-cycle-offset N`: skip B's first `N` cycles, so B's local edge `N`
//!   becomes logical cycle 0.
//!
//! Comparison units are numbered in **A's logical cycle space**; a B sample for
//! unit `c` is taken from B's local edge `c + origin_b`. All cycle numbers in
//! output (`cycle`, `a_cycle`, `b_cycle`) are logical, so they are directly
//! comparable; `b_cycle_local` is also reported for tracing back into B's dump.

use anyhow::{Result, bail};
use std::path::PathBuf;
use wellen::Timescale;
use wellen::simple::Waveform;

use crate::clock::{self, ClockInfo};
use crate::resolve::SigHandle;
use crate::sample::Cell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::A => "A",
            Side::B => "B",
        }
    }
}

/// One sampling view: a waveform plus the clock/origin it is read with.
pub struct SideView<'w> {
    pub side: Side,
    pub wf: &'w Waveform,
    pub clock: &'w ClockInfo,
    /// Local edge index that represents logical cycle 0.
    pub origin: i64,
    pub path: PathBuf,
    /// True when this view reads the same file as side A.
    pub shared: bool,
}

impl<'w> SideView<'w> {
    pub fn ts(&self) -> Option<Timescale> {
        self.wf.hierarchy().timescale()
    }

    pub fn local_of(&self, logical: i64) -> i64 {
        logical + self.origin
    }

    pub fn logical_of(&self, local: i64) -> i64 {
        local - self.origin
    }

    pub fn edges(&self) -> &'w [u64] {
        &self.clock.edges
    }

    /// Highest logical cycle with data in this file.
    pub fn last_logical_cycle(&self) -> i64 {
        self.clock.last_cycle() - self.origin
    }

    /// Tick of a logical cycle, when it exists in this file.
    pub fn tick_of(&self, logical: i64) -> Option<u64> {
        self.clock.tick_of(self.local_of(logical))
    }

    /// Sample a handle at a logical cycle (post-edge).
    pub fn cell(&self, handle: &SigHandle, logical: i64) -> Cell {
        let local = self.local_of(logical);
        handle.cell_at_cycle(self.wf, self.edges(), local)
    }

    pub fn is_one(&self, handle: &SigHandle, logical: i64) -> bool {
        self.cell(handle, logical).bits() == Some("1")
    }

    /// Highest logical cycle with data for any of the handles.
    pub fn last_data_cycle(&self, handles: &[&SigHandle]) -> i64 {
        let tt = self.wf.time_table();
        let mut last = -1i64;
        for handle in handles {
            if let Some(sig) = handle.signal(self.wf)
                && let Some(idx) = sig.time_indices().last()
                && let Some(tick) = tt.get(*idx as usize)
            {
                last = last.max(self.logical_of(self.clock.cycle_of(*tick)));
            }
        }
        last
    }
}

/// Both sides of a comparison.
pub struct Sides<'w> {
    pub a: SideView<'w>,
    pub b: SideView<'w>,
}

impl<'w> Sides<'w> {
    pub fn view(&self, side: Side) -> &SideView<'w> {
        match side {
            Side::A => &self.a,
            Side::B => &self.b,
        }
    }

    /// Sample a handle at a logical cycle, using the side it belongs to.
    pub fn cell(&self, handle: &SigHandle, logical: i64) -> Cell {
        self.view(handle.side).cell(handle, logical)
    }

    pub fn is_one(&self, handle: &SigHandle, logical: i64) -> bool {
        self.view(handle.side).is_one(handle, logical)
    }

    pub fn both_shared(&self) -> bool {
        self.b.shared
    }
}

/// Resolve the origin of one side: anchor (`first local cycle where NAME is 1`)
/// plus an explicit cycle offset.
pub fn resolve_origin(
    side: Side,
    view_wf: &Waveform,
    clock: &ClockInfo,
    anchor: Option<&SigHandle>,
    cycle_offset: i64,
) -> Result<i64> {
    let mut origin = cycle_offset;
    if let Some(handle) = anchor {
        let tt = view_wf.time_table();
        let mut found = None;
        for local in 0..=clock.last_cycle() {
            let tick = clock.edges[local as usize];
            if handle.cell_at_tick(view_wf, tick).bits() == Some("1") {
                found = Some(local);
                break;
            }
        }
        let Some(local) = found else {
            bail!(
                "--{}anchor {}: never asserted in the dump ({} ticks)",
                if side == Side::B { "b-" } else { "" },
                handle.full_name,
                tt.len()
            );
        };
        origin += local;
    }
    // A negative origin is legitimate: it labels cycles that precede the file's
    // first edge, which is how a re-based window (cut/fstclip) is read back with
    // the source's cycle numbers. Cycles without a local counterpart are
    // rejected by the bounds check, not here.
    Ok(origin)
}

/// Resolve the clock of one side and log which clock was used.
pub fn resolve_side_clock(
    wf: &mut Waveform,
    spec: Option<&str>,
    edge: crate::format::Edge,
    quiet: bool,
    side: Side,
) -> Result<ClockInfo> {
    let info = clock::resolve_clock(wf, spec, edge, quiet)?;
    match info {
        Some(info) => Ok(info),
        None => bail!(
            "side {} needs a clock: do not pass --clock none for comparisons",
            side.name()
        ),
    }
}
