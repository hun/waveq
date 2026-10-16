//! Expected-value sources for side B: a file of per-cycle golden values
//! (CSV or JSONL) instead of a waveform signal.
//!
//! Spec syntax (used as the right-hand side of `--map`, or via
//! `--b-expect`): `@PATH[:cycle_col,value_col]`
//!
//! ```text
//! --map dut.dout=@golden.csv:cycle,dout
//! --map dut.dout=@golden.jsonl:cycle,value
//! --map dut.dout=@golden.csv            # first two columns
//! ```
//!
//! * CSV: one row per sample, `#` starts a comment, blank lines are ignored.
//!   A first line whose fields are not all numbers is treated as the header, so
//!   columns can be named (`cycle,dout`) or referenced by 0-based index (`0,2`).
//! * JSONL/JSON: one object per line (an array of objects also works); columns
//!   are object keys.
//! * Values are integers (decimal, `0x..`, `0b..`, signed) or bit patterns
//!   (`0101xz`). Integers are widened to the compared signal's width, with the
//!   sign extension taken from that signal's signedness.
//! * Cycles are **logical cycles** of side A, i.e. the same numbering the rest
//!   of the output uses.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::format::{Radix, render};
use crate::sample::Val;

/// A parsed spec: `@PATH[:cycle_col,value_col]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectSpec {
    pub path: PathBuf,
    pub cycle_col: Option<String>,
    pub value_col: Option<String>,
}

impl ExpectSpec {
    pub fn label(&self) -> String {
        match (&self.cycle_col, &self.value_col) {
            (Some(c), Some(v)) => format!("@{}:{c},{v}", self.path.display()),
            _ => format!("@{}", self.path.display()),
        }
    }
}

/// Is this a spec that points at an expected-value file?
pub fn is_expect(spec: &str) -> bool {
    spec.starts_with('@')
}

pub fn parse_spec(spec: &str) -> Result<ExpectSpec> {
    let body = spec
        .strip_prefix('@')
        .ok_or_else(|| anyhow::anyhow!("expected \"@FILE[:cycle,value]\", got \"{spec}\""))?;
    if body.is_empty() {
        bail!("expected \"@FILE[:cycle,value]\", got \"{spec}\"");
    }
    // the column list is only a column list if it has no path separator after it
    if let Some((path, cols)) = body.rsplit_once(':')
        && !cols.is_empty()
        && !cols.contains(['/', '\\'])
    {
        let (c, v) = cols
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("@FILE columns expect CYCLE,VALUE, got \"{cols}\""))?;
        return Ok(ExpectSpec {
            path: PathBuf::from(path),
            cycle_col: Some(c.trim().to_string()),
            value_col: Some(v.trim().to_string()),
        });
    }
    Ok(ExpectSpec {
        path: PathBuf::from(body),
        cycle_col: None,
        value_col: None,
    })
}

/// One expected value by cycle.
#[derive(Debug, Clone, Default)]
pub struct ExpectTable {
    pub path: PathBuf,
    pub cycle_col: String,
    pub value_col: String,
    values: HashMap<i64, String>,
}

impl ExpectTable {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn cycles(&self) -> Vec<i64> {
        let mut out: Vec<i64> = self.values.keys().copied().collect();
        out.sort_unstable();
        out
    }

    /// First/last cycle in the file.
    pub fn range(&self) -> Option<(i64, i64)> {
        let cycles = self.cycles();
        Some((*cycles.first()?, *cycles.last()?))
    }

    /// The raw cell text for a cycle, if the file covers it.
    pub fn cell_text(&self, cycle: i64) -> Option<&str> {
        self.values.get(&cycle).map(|s| s.as_str())
    }

    /// Expected value as a `Val`, widened to `width` bits.
    pub fn value(&self, cycle: i64, width: u32, signed: bool) -> Result<Option<Val>> {
        match self.cell_text(cycle) {
            None => Ok(None),
            Some(text) => Ok(Some(parse_value(text, width, signed).with_context(
                || {
                    format!(
                        "{}: cycle {cycle} column \"{}\"",
                        self.path.display(),
                        self.value_col
                    )
                },
            )?)),
        }
    }
}

/// A cache of parsed tables, so several `--map` entries can share one file.
#[derive(Default)]
pub struct ExpectCache {
    tables: HashMap<(PathBuf, Option<String>, Option<String>), Rc<ExpectTable>>,
}

impl ExpectCache {
    pub fn load(&mut self, spec: &ExpectSpec) -> Result<Rc<ExpectTable>> {
        let key = (
            spec.path.clone(),
            spec.cycle_col.clone(),
            spec.value_col.clone(),
        );
        if let Some(t) = self.tables.get(&key) {
            return Ok(t.clone());
        }
        let table = Rc::new(read_table(spec)?);
        if table.is_empty() {
            bail!("{}: no samples found", spec.path.display());
        }
        self.tables.insert(key, table.clone());
        Ok(table)
    }
}

fn split_csv_line(line: &str) -> Vec<String> {
    line.split(',')
        .map(|f| f.trim().trim_matches('"').trim().to_string())
        .collect()
}

fn read_table(spec: &ExpectSpec) -> Result<ExpectTable> {
    let text = std::fs::read_to_string(&spec.path)
        .with_context(|| format!("cannot read {}", spec.path.display()))?;
    let trimmed = text.trim_start();
    let is_json = trimmed.starts_with('{') || trimmed.starts_with('[');
    if is_json {
        read_json(spec, &text)
    } else {
        read_csv(spec, &text)
    }
}

fn read_json(spec: &ExpectSpec, text: &str) -> Result<ExpectTable> {
    let mut rows: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();
    let whole: Option<serde_json::Value> = serde_json::from_str(text.trim()).ok();
    match whole {
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                match item {
                    serde_json::Value::Object(map) => rows.push(map),
                    other => bail!("{}: expected objects, got {other}", spec.path.display()),
                }
            }
        }
        Some(serde_json::Value::Object(map)) => rows.push(map),
        _ => {
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(line)
                    .with_context(|| format!("{}: line {}", spec.path.display(), n + 1))?;
                match value {
                    serde_json::Value::Object(map) => rows.push(map),
                    other => bail!(
                        "{}: line {}: expected an object, got {other}",
                        spec.path.display(),
                        n + 1
                    ),
                }
            }
        }
    }
    if rows.is_empty() {
        bail!("{}: no rows", spec.path.display());
    }
    let keys: Vec<String> = rows[0].keys().cloned().collect();
    let cycle_col = spec.cycle_col.clone().unwrap_or_else(|| {
        keys.iter()
            .find(|k| k.eq_ignore_ascii_case("cycle"))
            .cloned()
            .unwrap_or_else(|| keys[0].clone())
    });
    let value_col = spec.value_col.clone().unwrap_or_else(|| {
        keys.iter()
            .find(|k| !k.eq_ignore_ascii_case("cycle"))
            .cloned()
            .unwrap_or_else(|| keys.get(1).cloned().unwrap_or_default())
    });
    let mut table = ExpectTable {
        path: spec.path.clone(),
        cycle_col: cycle_col.clone(),
        value_col: value_col.clone(),
        values: HashMap::new(),
    };
    for (n, row) in rows.iter().enumerate() {
        let line = n + 1;
        let cycle_val = row.get(cycle_col.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "{}: row {line}: no column \"{cycle_col}\" (have: {})",
                spec.path.display(),
                row.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        let value_val = row.get(value_col.as_str()).ok_or_else(|| {
            anyhow::anyhow!(
                "{}: row {line}: no column \"{value_col}\"",
                spec.path.display()
            )
        })?;
        let cycle = json_cycle(cycle_val).with_context(|| {
            format!(
                "{}: row {line}: cycle column \"{cycle_col}\"",
                spec.path.display()
            )
        })?;
        insert(
            &mut table,
            cycle,
            json_value_text(value_val),
            &spec.path,
            line,
        )?;
    }
    Ok(table)
}

fn json_cycle(value: &serde_json::Value) -> Result<i64> {
    match value {
        serde_json::Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("not an integer: {n}")),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("not an integer: \"{s}\"")),
        other => bail!("not an integer: {other}"),
    }
}

fn json_value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn read_csv(spec: &ExpectSpec, text: &str) -> Result<ExpectTable> {
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let l = l.trim();
            !l.is_empty() && !l.starts_with('#') && !l.starts_with("//")
        })
        .peekable();
    let Some((first_no, first)) = lines.peek().copied() else {
        bail!("{}: empty file", spec.path.display());
    };
    // line numbers are 1-based over the original file; the header is the first
    // non-comment line, which is not necessarily line 1
    let header_line = first_no + 1;
    let header = split_csv_line(first);
    let has_header = header.iter().any(|f| {
        !f.is_empty() && f.parse::<i64>().is_err() && !f.starts_with("0x") && !f.starts_with("0b")
    });
    let (cycle_col, value_col) = match (&spec.cycle_col, &spec.value_col) {
        (Some(c), Some(v)) => (c.clone(), v.clone()),
        (None, None) if header.len() >= 2 => {
            if has_header {
                (header[0].clone(), header[1].clone())
            } else {
                ("0".to_string(), "1".to_string())
            }
        }
        (None, None) => bail!(
            "{}: need at least two columns (or give them: @{}:cycle,value)",
            spec.path.display(),
            spec.path.display()
        ),
        _ => bail!(
            "{}: give both columns (@FILE:cycle,value)",
            spec.path.display()
        ),
    };
    let header_index = |name: &str| -> Option<usize> {
        header
            .iter()
            .position(|h| h.eq_ignore_ascii_case(name))
            .or_else(|| name.parse::<usize>().ok())
    };
    let mut table = ExpectTable {
        path: spec.path.clone(),
        cycle_col: cycle_col.clone(),
        value_col: value_col.clone(),
        values: HashMap::new(),
    };
    for (n, line) in lines {
        let line_no = n + 1;
        if has_header && line_no == header_line {
            continue;
        }
        let fields = split_csv_line(line);
        let get = |name: &str| -> Result<&String> {
            let idx = header_index(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: line {line_no}: no column \"{name}\"{}",
                    spec.path.display(),
                    if has_header {
                        format!(" (have: {})", header.join(", "))
                    } else {
                        String::new()
                    }
                )
            })?;
            fields.get(idx).ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: line {line_no}: column \"{name}\" (index {idx}) is missing",
                    spec.path.display()
                )
            })
        };
        let cycle_text = get(&cycle_col)?;
        let cycle: i64 = cycle_text.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "{}: line {line_no}: cycle column \"{cycle_col}\" is not an integer: \"{cycle_text}\"",
                spec.path.display()
            )
        })?;
        let value_text = get(&value_col)?.clone();
        insert(&mut table, cycle, value_text, &spec.path, line_no)?;
    }
    Ok(table)
}

fn insert(
    table: &mut ExpectTable,
    cycle: i64,
    text: String,
    path: &Path,
    line: usize,
) -> Result<()> {
    if let Some(prev) = table.values.get(&cycle) {
        bail!(
            "{}: line {line}: cycle {cycle} already has a value (\"{prev}\"); one row per cycle per column",
            path.display()
        );
    }
    table.values.insert(cycle, text);
    Ok(())
}

/// Convert one expected-value cell into a `Val` of `width` bits.
pub fn parse_value(text: &str, width: u32, signed: bool) -> Result<Val> {
    let text = text.trim();
    if text.is_empty() {
        bail!("empty value");
    }
    // Integers win over bit patterns: "0x10" and "1010" are numbers, while
    // anything containing x/z (or prefixed with 0b) is a bit pattern.
    let lower_all = text.to_ascii_lowercase();
    let numeric = lower_all.starts_with("0x")
        || lower_all.starts_with("0b")
        || lower_all.starts_with('-')
        || lower_all.starts_with('+')
        || lower_all.starts_with('\'')
        || text.chars().all(|c| c.is_ascii_digit() || c == '_');
    if !numeric
        && text
            .chars()
            .all(|c| matches!(c, '0' | '1' | 'x' | 'X' | 'z' | 'Z' | '_'))
    {
        let bits: String = text.chars().filter(|c| *c != '_').collect();
        if bits.is_empty() {
            bail!("empty bit pattern");
        }
        return Ok(Val::Bits(bits));
    }
    let lower = text.to_ascii_lowercase();
    let (negative, body) = match lower.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, lower.clone()),
    };
    let (radix, digits) = if let Some(rest) = body.strip_prefix("0x") {
        (16u32, rest.to_string())
    } else if let Some(rest) = body.strip_prefix("0b") {
        (2, rest.to_string())
    } else if let Some(rest) = body.strip_prefix('\'') {
        match rest.chars().next() {
            Some('h') => (16, rest[1..].to_string()),
            Some('b') => (2, rest[1..].to_string()),
            Some('d') => (10, rest[1..].to_string()),
            _ => bail!("invalid value \"{text}\""),
        }
    } else {
        (10, body.clone())
    };
    let magnitude = i128::from_str_radix(&digits, radix)
        .map_err(|_| anyhow::anyhow!("invalid value \"{text}\""))?;
    let value = if negative { -magnitude } else { magnitude };
    // no silent truncation: a golden value that does not fit the compared
    // signal's width is a width mismatch worth reporting
    let width = width.max(1);
    let fits = if signed {
        let lo = -(1i128 << (width - 1));
        let hi = (1i128 << (width - 1)) - 1;
        value >= lo && value <= hi
    } else {
        value >= 0 && (width >= 128 || value < (1i128 << width))
    };
    if !fits {
        bail!(
            "value {value} does not fit in {width} bit{} ({})",
            if width == 1 { "" } else { "s" },
            if signed { "signed" } else { "unsigned" }
        );
    }
    Ok(Val::Bits(int_to_bits(value, width, signed)))
}

/// Two's-complement bit pattern of `value` in `width` bits.
pub fn int_to_bits(value: i128, width: u32, signed: bool) -> String {
    let width = width.max(1);
    let mask: i128 = if width >= 128 {
        -1
    } else {
        (1i128 << width) - 1
    };
    let raw = if signed || value < 0 {
        // sign-extend into the target width
        (value & mask) as u128
    } else {
        (value as u128) & (mask as u128)
    };
    let mut out = String::with_capacity(width as usize);
    for i in (0..width).rev() {
        let bit = (raw >> i) & 1;
        out.push(if bit == 1 { '1' } else { '0' });
    }
    out
}

/// Render an expected value for display (uses the caller's radix/signedness).
pub fn render_value(bits: &str, radix: Radix, signed: bool) -> String {
    render(bits, radix, signed).unwrap_or_else(|_| bits.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        let s = parse_spec("@golden.csv:cycle,dout").unwrap();
        assert_eq!(s.path, PathBuf::from("golden.csv"));
        assert_eq!(s.cycle_col.as_deref(), Some("cycle"));
        assert_eq!(s.value_col.as_deref(), Some("dout"));
        let s = parse_spec("@/tmp/g.jsonl:0,1").unwrap();
        assert_eq!(s.path, PathBuf::from("/tmp/g.jsonl"));
        let s = parse_spec("@golden.csv").unwrap();
        assert_eq!(s.cycle_col, None);
        assert!(parse_spec("golden.csv").is_err());
    }

    #[test]
    fn values() {
        assert_eq!(
            parse_value("5", 8, false).unwrap(),
            Val::Bits("00000101".into())
        );
        assert_eq!(
            parse_value("-1", 8, true).unwrap(),
            Val::Bits("11111111".into())
        );
        assert_eq!(
            parse_value("0xff", 8, false).unwrap(),
            Val::Bits("11111111".into())
        );
        assert_eq!(
            parse_value("0b1010", 4, false).unwrap(),
            Val::Bits("1010".into())
        );
        assert_eq!(
            parse_value("10xz", 4, false).unwrap(),
            Val::Bits("10xz".into())
        );
        // plain digits are decimal, not a bit pattern, and must fit
        assert_eq!(
            parse_value("1010", 12, false).unwrap(),
            Val::Bits("001111110010".into())
        );
        assert!(parse_value("1010", 8, false).is_err());
        assert!(parse_value("-1", 8, false).is_err());
        assert_eq!(
            parse_value("0b1010", 8, false).unwrap(),
            Val::Bits("00001010".into())
        );
        // widening keeps the value: sign extension is driven by `signed`
        assert_eq!(int_to_bits(255, 9, false), "011111111");
        assert_eq!(int_to_bits(-1, 9, true), "111111111");
        assert!(parse_value("", 8, false).is_err());
        assert!(parse_value("nope", 8, false).is_err());
    }

    #[test]
    fn csv_and_jsonl() {
        let dir = std::env::temp_dir().join(format!("waveq_expect_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("golden.csv");
        std::fs::write(&csv, "# golden\ncycle,dout\n0,0\n1,4\n2,8\n\n3,0x10\n").unwrap();
        let mut cache = ExpectCache::default();
        let table = cache
            .load(&parse_spec(&format!("@{}", csv.display())).unwrap())
            .unwrap();
        assert_eq!(table.cycle_col, "cycle");
        assert_eq!(table.value_col, "dout");
        assert_eq!(table.len(), 4);
        assert_eq!(table.range(), Some((0, 3)));
        assert_eq!(
            table.value(3, 9, false).unwrap(),
            Some(Val::Bits("000010000".into()))
        );
        assert_eq!(table.value(9, 9, false).unwrap(), None);

        let jsonl = dir.join("golden.jsonl");
        std::fs::write(
            &jsonl,
            "{\"cycle\":0,\"value\":\"0000\"}\n{\"cycle\":1,\"value\":16}\n",
        )
        .unwrap();
        let table = cache
            .load(&parse_spec(&format!("@{}:cycle,value", jsonl.display())).unwrap())
            .unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(
            table.value(1, 9, false).unwrap(),
            Some(Val::Bits("000010000".into()))
        );

        // duplicate cycles in one column are refused
        let dup = dir.join("dup.csv");
        std::fs::write(&dup, "cycle,dout\n1,0\n1,1\n").unwrap();
        let err = cache
            .load(&parse_spec(&format!("@{}", dup.display())).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("already has a value"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
