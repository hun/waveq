//! Radix rendering, time parsing/formatting and text column layout.
//!
//! All tick arithmetic is exact integer arithmetic: a tick is
//! `timescale.factor * 10^(timescale_unit_exponent)` femtoseconds, which is
//! represented in zeptoseconds (`factor * 10^(exp + 21)`) so that every
//! timescale GHDL/VCD/FST can express stays integral.

use anyhow::{Result, anyhow, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use wellen::{Timescale, TimescaleUnit};

/// Output radix for bit-vector signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Radix {
    Auto,
    Bin,
    Hex,
    Oct,
    Dec,
}

impl Radix {
    pub fn name(self) -> &'static str {
        match self {
            Radix::Auto => "auto",
            Radix::Bin => "bin",
            Radix::Hex => "hex",
            Radix::Oct => "oct",
            Radix::Dec => "dec",
        }
    }
}

/// Clock edge selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Edge {
    Posedge,
    Negedge,
    Both,
}

impl Edge {
    pub fn name(self) -> &'static str {
        match self {
            Edge::Posedge => "posedge",
            Edge::Negedge => "negedge",
            Edge::Both => "both",
        }
    }
}

/// How compared units (cycles or transfers) are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlignMode {
    Cycles,
    Transfers,
}

impl AlignMode {
    pub fn name(self) -> &'static str {
        match self {
            AlignMode::Cycles => "cycles",
            AlignMode::Transfers => "transfers",
        }
    }
}

/// Value comparison mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CmpMode {
    BitExact,
    Numeric,
}

impl CmpMode {
    pub fn name(self) -> &'static str {
        match self {
            CmpMode::BitExact => "bit-exact",
            CmpMode::Numeric => "numeric",
        }
    }
}

/// How an `x`/`z`-involved comparison is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum XMode {
    Mismatch,
    Wildcard,
}

impl XMode {
    pub fn name(self) -> &'static str {
        match self {
            XMode::Mismatch => "mismatch",
            XMode::Wildcard => "wildcard",
        }
    }
}

// ---------------------------------------------------------------------------
// time
// ---------------------------------------------------------------------------

const TIME_UNITS: [(&str, i8); 6] = [
    ("s", 0),
    ("ms", -3),
    ("us", -6),
    ("ns", -9),
    ("ps", -12),
    ("fs", -15),
];

fn zs_per_unit(exponent: i8) -> i128 {
    10i128.pow((exponent + 21) as u32)
}

/// Zeptoseconds per tick, or `None` when the file carries no usable timescale.
fn tick_zs(ts: Option<Timescale>) -> Option<i128> {
    let ts = ts?;
    let exp = ts.unit.to_exponent()?;
    Some(ts.factor as i128 * zs_per_unit(exp))
}

pub fn timscale_unit_name(unit: TimescaleUnit) -> &'static str {
    match unit {
        TimescaleUnit::ZeptoSeconds => "zs",
        TimescaleUnit::AttoSeconds => "as",
        TimescaleUnit::FemtoSeconds => "fs",
        TimescaleUnit::PicoSeconds => "ps",
        TimescaleUnit::NanoSeconds => "ns",
        TimescaleUnit::MicroSeconds => "us",
        TimescaleUnit::MilliSeconds => "ms",
        TimescaleUnit::Seconds => "s",
        TimescaleUnit::Unknown => "?",
    }
}

pub fn timescale_text(ts: Option<Timescale>) -> String {
    match ts {
        None => "unknown".to_string(),
        Some(ts) => format!("{}{}", ts.factor, timscale_unit_name(ts.unit)),
    }
}

/// The frequency of one tick as a re-usable text, for error messages.
fn tick_description(ts: Timescale) -> String {
    format!("{}{}", ts.factor, timscale_unit_name(ts.unit))
}

/// Parse `10ns` / `1200` into a tick count.
///
/// A bare integer is a tick count. `<N><unit>` is converted exactly; a
/// conversion that is not integral is an error (the tool never rounds times).
pub fn parse_time(s: &str, ts: Option<Timescale>) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty time");
    }
    if let Ok(ticks) = s.parse::<u64>() {
        return Ok(ticks);
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| {
        anyhow!("invalid time \"{s}\": expected <N><unit> (10ns) or a tick count")
    })?;
    if split == 0 {
        bail!("invalid time \"{s}\": expected <N><unit> (10ns) or a tick count");
    }
    let n: i128 = s[..split]
        .parse()
        .map_err(|_| anyhow!("invalid time \"{s}\": number too large"))?;
    let unit = &s[split..];
    let exp = TIME_UNITS
        .iter()
        .find(|(u, _)| *u == unit)
        .map(|(_, e)| *e)
        .ok_or_else(|| {
            anyhow!(
                "invalid time unit \"{unit}\" in \"{s}\": use one of {}",
                TIME_UNITS
                    .iter()
                    .map(|(u, _)| *u)
                    .collect::<Vec<_>>()
                    .join("|")
            )
        })?;
    let ts = ts.ok_or_else(|| anyhow!("file has no timescale; give times in ticks"))?;
    let per_tick =
        tick_zs(Some(ts)).ok_or_else(|| anyhow!("file has no timescale; give times in ticks"))?;
    let total = n * zs_per_unit(exp);
    if total % per_tick != 0 {
        bail!(
            "\"{s}\" is not an integral number of ticks (tick = {})",
            tick_description(ts)
        );
    }
    let ticks = total / per_tick;
    u64::try_from(ticks).map_err(|_| anyhow!("time \"{s}\" is out of range"))
}

/// Format a tick count using the largest time unit that divides exactly.
pub fn format_time(tick: u64, ts: Option<Timescale>) -> String {
    let Some(per_tick) = tick_zs(ts) else {
        return format!("{tick} ticks");
    };
    let total_zs = tick as i128 * per_tick;
    for (name, exp) in TIME_UNITS {
        let per_unit = zs_per_unit(exp);
        if total_zs % per_unit == 0 {
            return format!("{}{}", total_zs / per_unit, name);
        }
    }
    format!("{tick} ticks")
}

// ---------------------------------------------------------------------------
// bit-vector rendering
// ---------------------------------------------------------------------------

pub fn has_xz(bits: &str) -> bool {
    bits.chars().any(|c| matches!(c, 'x' | 'X' | 'z' | 'Z'))
}

fn group_digit(group: &[char], base: u32) -> char {
    if group.iter().all(|c| matches!(c, 'z' | 'Z')) {
        return 'z';
    }
    if group.iter().any(|c| matches!(c, 'x' | 'X' | 'z' | 'Z')) {
        return 'x';
    }
    let mut val: u32 = 0;
    for c in group {
        val = val * 2 + u32::from(*c == '1');
    }
    char::from_digit(val, base).unwrap_or('?')
}

fn group_from_lsb(bits: &str, group: usize, base: u32) -> String {
    let chars: Vec<char> = bits.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len() / group + 1);
    let mut end = chars.len();
    while end > 0 {
        let start = end.saturating_sub(group);
        out.push(group_digit(&chars[start..end], base));
        end = start;
    }
    out.reverse();
    out.into_iter().collect()
}

/// Render a canonical MSB-first bit string in the requested radix.
pub fn render(bits: &str, radix: Radix, signed: bool) -> Result<String> {
    let width = bits.chars().count();
    let radix = match radix {
        Radix::Auto => {
            if width <= 1 {
                Radix::Bin
            } else {
                Radix::Hex
            }
        }
        other => other,
    };
    Ok(match radix {
        Radix::Bin | Radix::Auto => bits.to_string(),
        Radix::Hex => group_from_lsb(bits, 4, 16),
        Radix::Oct => group_from_lsb(bits, 3, 8),
        Radix::Dec => {
            if width > 64 {
                bail!("decimal rendering requires width <= 64 (has {width}); use --radix hex");
            }
            if has_xz(bits) {
                "x".to_string()
            } else {
                let mut raw: u64 = 0;
                for c in bits.chars() {
                    raw = (raw << 1) | u64::from(c == '1');
                }
                if signed && width > 0 {
                    let shift = 64 - width;
                    (((raw << shift) as i64) >> shift).to_string()
                } else {
                    raw.to_string()
                }
            }
        }
    })
}

/// Extract the `[msb:lsb]` part of a canonical MSB-first bit string.
pub fn slice_bits(bits: &str, msb: u32, lsb: u32) -> Result<String> {
    let width = bits.chars().count() as u32;
    if msb >= width {
        bail!("bit range [{msb}:{lsb}] is out of range for a {width}-bit signal");
    }
    let chars: Vec<char> = bits.chars().collect();
    let hi = width - 1 - msb;
    let lo = width - 1 - lsb;
    Ok(chars[hi as usize..=lo as usize].iter().collect())
}

/// Unsigned integer value of a canonical MSB-first bit string (`0` for x/z).
pub fn bits_to_u64(bits: &str) -> u64 {
    let mut raw: u64 = 0;
    for c in bits.chars() {
        raw = (raw << 1) | u64::from(c == '1');
    }
    raw
}

// ---------------------------------------------------------------------------
// text tables
// ---------------------------------------------------------------------------

fn display_len(s: &str) -> usize {
    s.chars().count()
}

/// Left-aligned columns, single-space separated, trailing blanks trimmed.
pub fn format_table(columns: &[String], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = columns.iter().map(|c| display_len(c)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(display_len(cell));
            }
        }
    }
    let mut out = String::new();
    let emit = |cells: &[String], out: &mut String| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            line.push_str(cell);
            if i + 1 < cells.len() {
                for _ in display_len(cell)..widths[i] {
                    line.push(' ');
                }
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };
    emit(columns, &mut out);
    for row in rows {
        emit(row, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(factor: u32, unit: TimescaleUnit) -> Option<Timescale> {
        Some(Timescale::new(factor, unit))
    }

    #[test]
    fn parse_time_ticks_and_units() {
        let ns1 = ts(1, TimescaleUnit::NanoSeconds);
        assert_eq!(parse_time("1200", ns1).unwrap(), 1200);
        assert_eq!(parse_time("10ns", ns1).unwrap(), 10);
        assert_eq!(parse_time("1us", ns1).unwrap(), 1000);
        assert_eq!(parse_time("5s", ns1).unwrap(), 5_000_000_000);
        assert!(parse_time("7ps", ns1).is_err());

        let fs1 = ts(1, TimescaleUnit::FemtoSeconds);
        assert_eq!(parse_time("25ns", fs1).unwrap(), 25_000_000);

        let ps10 = ts(10, TimescaleUnit::PicoSeconds);
        assert_eq!(parse_time("1ns", ps10).unwrap(), 100);
        assert!(parse_time("1ps", ps10).is_err());

        assert!(parse_time("10ns", None).is_err());
        assert_eq!(parse_time("42", None).unwrap(), 42);
        assert!(parse_time("10zz", ns1).is_err());
    }

    #[test]
    fn format_time_exact() {
        let ns1 = ts(1, TimescaleUnit::NanoSeconds);
        assert_eq!(format_time(135, ns1), "135ns");
        assert_eq!(format_time(1000, ns1), "1us");
        assert_eq!(format_time(0, ns1), "0s");
        assert_eq!(format_time(7, None), "7 ticks");
        let ps10 = ts(10, TimescaleUnit::PicoSeconds);
        assert_eq!(format_time(100, ps10), "1ns");
        assert_eq!(format_time(1, ps10), "10ps");
    }

    #[test]
    fn render_radices() {
        assert_eq!(render("11111101", Radix::Bin, false).unwrap(), "11111101");
        assert_eq!(render("11111101", Radix::Dec, false).unwrap(), "253");
        assert_eq!(render("11111101", Radix::Dec, true).unwrap(), "-3");
        assert_eq!(render("11111101", Radix::Hex, false).unwrap(), "fd");
        assert_eq!(render("11111101", Radix::Auto, false).unwrap(), "fd");
        assert_eq!(render("0", Radix::Auto, false).unwrap(), "0");
        // partial top group uses only its own bits
        assert_eq!(
            render("1_0001".replace('_', "").as_str(), Radix::Hex, false).unwrap(),
            "11"
        );
        assert_eq!(render("101", Radix::Hex, false).unwrap(), "5");
        assert_eq!(render("101", Radix::Oct, false).unwrap(), "5");
        assert_eq!(render("1010", Radix::Oct, false).unwrap(), "12");
        assert_eq!(render("1xz0", Radix::Hex, false).unwrap(), "x");
        assert_eq!(render("zzzz", Radix::Hex, false).unwrap(), "z");
        assert_eq!(render("x000", Radix::Hex, false).unwrap(), "x");
        // groups are rendered independently: "11" -> 3, "11z0" -> x
        assert_eq!(render("1111z0", Radix::Hex, false).unwrap(), "3x");
        assert_eq!(render("1x0", Radix::Dec, false).unwrap(), "x");
        let wide = "1".repeat(65);
        assert!(render(&wide, Radix::Dec, false).is_err());
        assert!(render(&wide, Radix::Hex, false).is_ok());
    }

    #[test]
    fn slice_and_value() {
        assert_eq!(slice_bits("10110100", 7, 4).unwrap(), "1011");
        assert_eq!(slice_bits("10110100", 3, 0).unwrap(), "0100");
        assert_eq!(slice_bits("10110100", 2, 2).unwrap(), "1");
        assert!(slice_bits("101", 3, 0).is_err());
        assert_eq!(bits_to_u64("000101101"), 45);
    }

    #[test]
    fn table_layout() {
        let cols = vec!["cycle".to_string(), "sum".to_string()];
        let rows = vec![
            vec!["0".to_string(), "4".to_string()],
            vec!["13".to_string(), "56".to_string()],
        ];
        assert_eq!(format_table(&cols, &rows), "cycle sum\n0     4\n13    56\n");
    }
}
