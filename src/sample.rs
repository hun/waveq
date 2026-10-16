//! Sampling: canonical values at a tick or cycle, with post-edge convention.
//!
//! `cycle c` samples at the tick of edge `c` and takes the *last* value at or
//! before that tick, so a register loaded exactly at the edge is visible in
//! that cycle. Same-tick delta-cycle values collapse to the last element,
//! matching the value a real simulator would settle on.

use anyhow::Result;
use wellen::simple::Waveform;
use wellen::{Signal, SignalValueRef};

use crate::format::{Radix, has_xz, render};
use crate::resolve::SigHandle;

/// An owned signal value.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Bits(String),
    Real(f64),
    Str(String),
    Event,
}

impl Val {
    pub fn from_ref(v: SignalValueRef<'_>) -> Val {
        match v {
            SignalValueRef::Event => Val::Event,
            SignalValueRef::BitVec(b) => Val::Bits(b.bit_string()),
            SignalValueRef::String(s) => Val::Str(s.to_string()),
            SignalValueRef::Real(r) => Val::Real(r),
        }
    }

    pub fn bits(&self) -> Option<&str> {
        match self {
            Val::Bits(b) => Some(b),
            _ => None,
        }
    }

    pub fn has_xz(&self) -> bool {
        match self {
            Val::Bits(b) => has_xz(b),
            _ => false,
        }
    }

    pub fn render(&self, radix: Radix, signed: bool) -> Result<String> {
        match self {
            Val::Bits(b) => render(b, radix, signed),
            Val::Real(r) => Ok(format_real(*r)),
            Val::Str(s) => Ok(s.clone()),
            Val::Event => Ok("event".to_string()),
        }
    }

    /// The bit string, `None` for reals/strings/events.
    pub fn json_bits(&self) -> serde_json::Value {
        match self {
            Val::Bits(b) => b.clone().into(),
            _ => serde_json::Value::Null,
        }
    }

    pub fn json(&self, radix: Radix, signed: bool) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert(
            "value".into(),
            self.render(radix, signed)
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null),
        );
        m.insert("bits".into(), self.json_bits());
        m.insert("has_x".into(), self.has_xz().into());
        serde_json::Value::Object(m)
    }

    /// Bit-exact comparison payload: canonical bits, or a textual fallback.
    pub fn cmp_key(&self) -> String {
        match self {
            Val::Bits(b) => b.clone(),
            Val::Real(r) => format_real(*r),
            Val::Str(s) => s.clone(),
            Val::Event => "event".to_string(),
        }
    }
}

pub fn format_real(r: f64) -> String {
    if r == r.trunc() && r.abs() < 1e15 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// A sampled value plus provenance.
#[derive(Clone, Debug, Default)]
pub struct Cell {
    pub val: Option<Val>,
    /// Tick of the change that produced `val`.
    pub change_tick: Option<u64>,
}

impl Cell {
    pub fn json(&self, radix: Radix, signed: bool) -> serde_json::Value {
        match &self.val {
            Some(v) => v.json(radix, signed),
            None => serde_json::Value::Null,
        }
    }

    pub fn text(&self, radix: Radix, signed: bool) -> String {
        match &self.val {
            Some(v) => v.render(radix, signed).unwrap_or_else(|e| e.to_string()),
            None => "-".to_string(),
        }
    }

    pub fn bits(&self) -> Option<&str> {
        self.val.as_ref().and_then(|v| v.bits())
    }
}

/// A raw sample (bit string + raw value + change tick) at a tick.
pub struct SigSample<'a> {
    pub bits: Option<String>,
    pub value: SignalValueRef<'a>,
    pub change_tick: Option<u64>,
}

/// Index of the last time-table entry at or before `tick`, if any.
fn tt_index_at(tt: &[u64], tick: u64) -> Option<u32> {
    let n = tt.partition_point(|t| *t <= tick);
    if n == 0 { None } else { Some((n - 1) as u32) }
}

/// Sample the value of `sig` at `tick` (post-change convention).
pub fn sample_at_tick<'a>(wf: &'a Waveform, sig: &'a Signal, tick: u64) -> Option<SigSample<'a>> {
    let tt = wf.time_table();
    let idx = tt_index_at(tt, tick)?;
    let off = sig.get_offset(idx)?;
    let element = off.elements.checked_sub(1)?;
    let value = sig.get_value_at(&off, element);
    Some(SigSample {
        bits: value.to_bit_string(),
        value,
        change_tick: Some(tt[off.start]),
    })
}

/// Sample at the tick of clock cycle `cycle`.
pub fn sample_at_cycle<'a>(
    wf: &'a Waveform,
    sig: &'a Signal,
    edges: &[u64],
    cycle: i64,
) -> Option<SigSample<'a>> {
    let tick = *edges.get(usize::try_from(cycle).ok()?)?;
    sample_at_tick(wf, sig, tick)
}

impl SigHandle {
    pub fn signal<'a>(&self, wf: &'a Waveform) -> Option<&'a Signal> {
        wf.get_signal(self.signal_ref)
    }

    /// Load just this signal into the waveform.
    pub fn load(&self, wf: &mut Waveform) {
        wf.load_signals_multi_threaded(&[self.signal_ref]);
    }

    /// Sample and apply the requested bit slice.
    pub fn cell_at_tick(&self, wf: &Waveform, tick: u64) -> Cell {
        let Some(sig) = self.signal(wf) else {
            return Cell::default();
        };
        let Some(sample) = sample_at_tick(wf, sig, tick) else {
            return Cell::default();
        };
        let val = Val::from_ref(sample.value);
        let val = match val {
            Val::Bits(b) => match self.apply_slice(&b) {
                Ok(sliced) => Val::Bits(sliced),
                Err(_) => Val::Bits(b),
            },
            other => other,
        };
        Cell {
            val: Some(val),
            change_tick: sample.change_tick,
        }
    }

    pub fn cell_at_cycle(&self, wf: &Waveform, edges: &[u64], cycle: i64) -> Cell {
        let Ok(idx) = usize::try_from(cycle) else {
            return Cell::default();
        };
        match edges.get(idx) {
            Some(tick) => self.cell_at_tick(wf, *tick),
            None => Cell::default(),
        }
    }

    /// Per-cycle cells over the inclusive cycle range; errors are impossible,
    /// so out-of-range cycles simply yield empty cells.
    pub fn series(&self, wf: &Waveform, edges: &[u64], from: i64, to: i64) -> Vec<Cell> {
        if to < from {
            return Vec::new();
        }
        (from..=to)
            .map(|c| self.cell_at_cycle(wf, edges, c))
            .collect()
    }
}

/// Tick of the change that last set the sampled value.
pub fn change_tick_at(wf: &Waveform, sig: &Signal, tick: u64) -> Option<u64> {
    sample_at_tick(wf, sig, tick).and_then(|s| s.change_tick)
}

pub fn change_count(sig: &Signal) -> usize {
    sig.time_indices().len()
}

pub fn distinct_values(sig: &Signal) -> usize {
    let mut set: Vec<String> = Vec::new();
    for (_, v) in sig.iter_changes() {
        let key = Val::from_ref(v).cmp_key();
        if !set.contains(&key) {
            set.push(key);
        }
    }
    set.len()
}

pub fn last_change_tick(wf: &Waveform, sig: &Signal) -> Option<u64> {
    let tt = wf.time_table();
    let last = *sig.time_indices().last()?;
    tt.get(last as usize).copied()
}

pub fn first_change_tick(wf: &Waveform, sig: &Signal) -> Option<u64> {
    let tt = wf.time_table();
    let first = *sig.time_indices().first()?;
    tt.get(first as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tt_index_boundaries() {
        let tt = vec![0u64, 5, 15, 25];
        assert_eq!(tt_index_at(&tt, 0), Some(0));
        assert_eq!(tt_index_at(&tt, 4), Some(0));
        assert_eq!(tt_index_at(&tt, 5), Some(1));
        assert_eq!(tt_index_at(&tt, 25), Some(3));
        assert_eq!(tt_index_at(&tt, 1000), Some(3));
    }

    #[test]
    fn val_render() {
        assert_eq!(
            Val::Bits("000101101".into())
                .render(Radix::Dec, false)
                .unwrap(),
            "45"
        );
        assert_eq!(Val::Real(1.5).render(Radix::Auto, false).unwrap(), "1.5");
        assert_eq!(Val::Real(-3.0).render(Radix::Auto, false).unwrap(), "-3");
        assert_eq!(
            Val::Str("hello".into()).render(Radix::Auto, false).unwrap(),
            "hello"
        );
        assert_eq!(Val::Event.render(Radix::Auto, false).unwrap(), "event");
        assert!(Val::Bits("1x01".into()).has_xz());
    }
}
