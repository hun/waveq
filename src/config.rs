//! Reusable TOML configuration: discovery, parsing, and precedence helpers.
//!
//! Precedence, highest first:
//! explicit CLI option > named entry (`--check`/`--query`/`@group`) >
//! config section default > built-in default.

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::format::{AlignMode, CmpMode, Edge, Radix, XMode};

pub const CONFIG_FILENAME: &str = ".waveq.toml";

/// One `[types]` entry: per-signal rendering/signedness overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radix: Option<Radix>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaveCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<Edge>,
    /// Second dump holding side B (default: side B is in `file`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b_clock: Option<String>,
    /// Skip the first N cycles of side B
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b_cycle_offset: Option<i64>,
    /// Skip the first N cycles of side A
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_offset: Option<i64>,
    /// First cycle where this signal is 1 becomes logical cycle 0 of side A
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// Same, for side B
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b_anchor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompareCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<AlignMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmp: Option<CmpMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<XMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_shift: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_valid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_ready: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_valid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_ready: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// Side A glob -> side B glob, paired by leaf name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairs: Option<IndexMap<String, String>>,
    /// Default expected-value file for `--map A=@:cycle,value` B sides
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_file: Option<String>,
    /// Add delta/bits_differ columns
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<bool>,
    /// Treat |b - a| > N as a divergence
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_at: Option<ProbeAtCfg>,
}

/// Where `--probe` samples (config spelling of the CLI enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeAtCfg {
    ACycle,
    BCycle,
}

/// A named `compare` invocation (`[checks] NAME`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shift: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_shift: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_shift: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<AlignMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmp: Option<CmpMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<XMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_valid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_ready: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_valid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out_ready: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_cycle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_cycle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_at: Option<ProbeAtCfg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_edges_only: Option<bool>,
    /// Side A glob -> side B glob, paired by leaf name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairs: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map_glob: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair_scopes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair_by: Option<String>,
}

/// A named `table` invocation (`[queries] NAME`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signals: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycles: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_cycle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_cycle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_cycle: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave: Option<WaveCfg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<CompareCfg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub types: Option<IndexMap<String, TypeOverride>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signals: Option<IndexMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<IndexMap<String, CheckCfg>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queries: Option<IndexMap<String, QueryCfg>>,
}

/// Ordered `[types]` table with exact-then-glob lookup semantics.
#[derive(Debug, Clone, Default)]
pub struct TypeTable {
    entries: Vec<(String, TypeOverride)>,
}

impl TypeTable {
    pub fn from_config(cfg: &Config) -> TypeTable {
        let entries = cfg
            .types
            .as_ref()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        TypeTable { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[(String, TypeOverride)] {
        &self.entries
    }

    /// `spec` is the requested signal spec (`NAME` or `NAME[msb:lsb]`),
    /// `full_name` the resolved hierarchy name. An exact key match (against
    /// either form) wins, otherwise the first glob matching either form.
    pub fn lookup(&self, spec: &str, full_name: &str) -> Option<&TypeOverride> {
        let candidates = [full_name, spec];
        for (key, entry) in &self.entries {
            if candidates.iter().any(|c| c == key) {
                return Some(entry);
            }
        }
        for (key, entry) in &self.entries {
            let Ok(pattern) = glob::Pattern::new(key) else {
                continue;
            };
            if candidates.iter().any(|c| pattern.matches(c)) {
                return Some(entry);
            }
        }
        None
    }

    /// Validate every key as a glob pattern so typos fail loudly.
    pub fn validate(&self) -> Result<()> {
        for (key, _) in &self.entries {
            if glob::Pattern::new(key).is_err() && !key.contains('[') {
                bail!("invalid pattern \"{key}\" in [types]");
            }
        }
        Ok(())
    }
}

impl Config {
    pub fn wave_file(&self) -> Option<&str> {
        self.wave.as_ref().and_then(|w| w.file.as_deref())
    }

    pub fn wave_clock(&self) -> Option<&str> {
        self.wave.as_ref().and_then(|w| w.clock.as_deref())
    }

    pub fn wave_b_file(&self) -> Option<&str> {
        self.wave.as_ref().and_then(|w| w.b_file.as_deref())
    }

    pub fn wave_b_clock(&self) -> Option<&str> {
        self.wave.as_ref().and_then(|w| w.b_clock.as_deref())
    }

    pub fn wave_b_cycle_offset(&self) -> Option<i64> {
        self.wave.as_ref().and_then(|w| w.b_cycle_offset)
    }

    pub fn wave_cycle_offset(&self) -> Option<i64> {
        self.wave.as_ref().and_then(|w| w.cycle_offset)
    }

    pub fn wave_anchor(&self) -> Option<&str> {
        self.wave.as_ref().and_then(|w| w.anchor.as_deref())
    }

    pub fn wave_b_anchor(&self) -> Option<&str> {
        self.wave.as_ref().and_then(|w| w.b_anchor.as_deref())
    }

    pub fn wave_edge(&self) -> Option<Edge> {
        self.wave.as_ref().and_then(|w| w.edge)
    }

    pub fn signal_group(&self, name: &str) -> Option<&Vec<String>> {
        self.signals.as_ref().and_then(|m| m.get(name))
    }

    pub fn group_names(&self) -> Vec<String> {
        self.signals
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn check(&self, name: &str) -> Option<&CheckCfg> {
        self.checks.as_ref().and_then(|m| m.get(name))
    }

    pub fn check_names(&self) -> Vec<String> {
        self.checks
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn query(&self, name: &str) -> Option<&QueryCfg> {
        self.queries.as_ref().and_then(|m| m.get(name))
    }

    pub fn query_names(&self) -> Vec<String> {
        self.queries
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Rewrites serde's `unknown field` message into waveq's wording.
fn config_error(path: &Path, err: &str) -> anyhow::Error {
    if let Some(rest) = err.split("unknown field `").nth(1)
        && let Some(key) = rest.split('`').next()
    {
        return anyhow!("unknown config key \"{key}\" in {}", path.display());
    }
    anyhow!("invalid config {}: {}", path.display(), err.trim())
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| config_error(path, &e.to_string()))?;
    TypeTable::from_config(&cfg).validate()?;
    Ok(cfg)
}

/// First `.waveq.toml` from `start` upward; stops after the first ancestor
/// that contains `.git` (that directory is still searched).
pub fn discover(start: &Path) -> Option<PathBuf> {
    let mut dir = std::fs::canonicalize(start).ok()?;
    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        let cfg: Config =
            toml::from_str(text).map_err(|e| config_error(Path::new("t.toml"), &e.to_string()))?;
        Ok(cfg)
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = parse("[wave]\nbogus = 1\n").unwrap_err().to_string();
        assert!(err.contains("unknown config key \"bogus\""), "{err}");
        let err = parse("[nope]\nx = 1\n").unwrap_err().to_string();
        assert!(err.contains("unknown config key \"nope\""), "{err}");
    }

    #[test]
    fn full_schema_parses() {
        let cfg = parse(
            r#"
[wave]
file = "sim/dump.fst"
clock = "tb_top.clk"
edge = "negedge"

[compare]
align = "transfers"
cmp = "numeric"
x = "wildcard"
max_shift = 4
in_valid = "tb_top.in_valid"
out_valid = "tb_top.ov"

[types]
"tb_top.sn" = { signed = true, radix = "dec" }
"tb_top.q*" = { radix = "hex" }
"tb_top.bus[15:8]" = { radix = "bin" }

[signals]
inputs = ["tb_top.a", "tb_top.b", "tb_top.in_valid"]

[checks]
out_delay = { a = "tb_top.ref_y", b = "tb_top.q3", shift = 3, from_cycle = 0, to_cycle = 19 }

[queries]
pipeline = { signals = ["tb_top.ref_y"], context = 3 }
"#,
        )
        .unwrap();
        assert_eq!(cfg.wave_file(), Some("sim/dump.fst"));
        assert_eq!(cfg.wave_edge(), Some(Edge::Negedge));
        assert_eq!(
            cfg.compare.as_ref().unwrap().align,
            Some(AlignMode::Transfers)
        );
        assert_eq!(cfg.check("out_delay").unwrap().shift, Some(3));
        assert_eq!(cfg.signal_group("inputs").unwrap().len(), 3);
        assert_eq!(cfg.query("pipeline").unwrap().context, Some(3));
        let types = TypeTable::from_config(&cfg);
        assert_eq!(
            types.lookup("tb_top.sn", "tb_top.sn").unwrap().signed,
            Some(true)
        );
        assert_eq!(
            types.lookup("tb_top.q3", "tb_top.q3").unwrap().radix,
            Some(Radix::Hex)
        );
        // glob keys are tried in file order
        assert_eq!(
            types
                .lookup("tb_top.bus[15:8]", "tb_top.bus")
                .unwrap()
                .radix,
            Some(Radix::Bin)
        );
    }
}
