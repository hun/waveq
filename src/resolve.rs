//! Signal-name resolution: name specs, `[msb:lsb]` slices, glob filters and
//! signedness/radix precedence.

use anyhow::{Result, bail};
use wellen::{
    Hierarchy, ItemRef, SignalEncoding, SignalRef, VarDirection, VarIndex, VarRef, VarType,
};

use crate::config::TypeTable;
use crate::format::{Radix, render, slice_bits};

/// A signal reference: `NAME` or `NAME[msb:lsb]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigSpec {
    pub name: String,
    pub bits: Option<(u32, u32)>,
}

impl SigSpec {
    pub fn with_bits(&self) -> String {
        match self.bits {
            Some((msb, lsb)) => format!("{}[{}:{}]", self.name, msb, lsb),
            None => self.name.clone(),
        }
    }
}

/// Split a trailing `[msb:lsb]`. Anything else (including VHDL names that
/// merely contain brackets) is kept verbatim.
pub fn parse_sigspec(spec: &str) -> Result<SigSpec> {
    if let Some(open) = spec.rfind('[')
        && spec.ends_with(']')
    {
        let inner = &spec[open + 1..spec.len() - 1];
        if let Some((a, b)) = inner.split_once(':') {
            let a = a.trim();
            let b = b.trim();
            let looks_numeric = !a.is_empty()
                && !b.is_empty()
                && a.chars().all(|c| c.is_ascii_digit())
                && b.chars().all(|c| c.is_ascii_digit());
            if looks_numeric {
                let msb: u32 = a.parse()?;
                let lsb: u32 = b.parse()?;
                if msb < lsb {
                    bail!("invalid bit range \"[{inner}]\": msb must be >= lsb");
                }
                let name = spec[..open].to_string();
                if name.is_empty() {
                    bail!("invalid signal spec \"{spec}\": missing name before \"[{inner}]\"");
                }
                return Ok(SigSpec {
                    name,
                    bits: Some((msb, lsb)),
                });
            }
        }
    }
    Ok(SigSpec {
        name: spec.to_string(),
        bits: None,
    })
}

pub fn var_full_name(h: &Hierarchy, v: VarRef) -> String {
    h[v].full_name(h)
}

fn split_name(name: &str) -> (Vec<String>, String) {
    match name.rfind('.') {
        Some(pos) => (
            name[..pos].split('.').map(|s| s.to_string()).collect(),
            name[pos + 1..].to_string(),
        ),
        None => (Vec::new(), name.to_string()),
    }
}

/// Up to five nearby signal names, for "did you mean" hints.
pub fn suggestions(h: &Hierarchy, name: &str) -> Vec<String> {
    let (path, last) = split_name(name);
    let needle = last.to_lowercase();
    let scope = path.join(".");
    let mut out: Vec<String> = Vec::new();
    let push = |out: &mut Vec<String>, full: String| {
        if !out.contains(&full) {
            out.push(full);
        }
    };
    // 1. names that contain the requested leaf name
    for v in h.all_vars() {
        let full = var_full_name(h, v);
        if full.to_lowercase().contains(&needle) {
            push(&mut out, full);
            if out.len() >= 5 {
                return out;
            }
        }
    }
    // 2. names in the same scope that share the first character of the leaf
    let Some(first) = needle.chars().next() else {
        return out;
    };
    for v in h.all_vars() {
        let v_scope = h[v]
            .full_name(h)
            .rsplit_once('.')
            .map(|(s, _)| s.to_string())
            .unwrap_or_default();
        let leaf = h[v].name(h).to_lowercase();
        if v_scope == scope && leaf.starts_with(first) {
            push(&mut out, var_full_name(h, v));
            if out.len() >= 5 {
                return out;
            }
        }
    }
    out
}

/// Resolve a signal spec. Returns the var and, when the caller must slice the
/// value itself, the `(msb, lsb)` pair. A `[msb:lsb]` that names an actual
/// per-bit-split var resolves to that var with no slicing.
pub fn find_var(h: &Hierarchy, spec: &SigSpec) -> Result<(VarRef, Option<(u32, u32)>)> {
    let (path, last) = split_name(&spec.name);
    if let Some((msb, lsb)) = spec.bits
        && let Some(v) = h.lookup_var_with_index(
            &path,
            last.clone(),
            &Some(VarIndex::new(i64::from(msb), i64::from(lsb))),
        )
    {
        return Ok((v, None));
    }
    if let Some(item) = h.lookup_item_by_name(&spec.name) {
        return match item {
            ItemRef::Scope(_) => bail!(
                "\"{}\" is a scope, not a signal. Use `waveq scopes` to list scopes or `waveq signals` to list signals in it.",
                spec.name
            ),
            ItemRef::Var(v) => {
                let exact = spec.bits.is_some_and(|(msb, lsb)| {
                    h[v].index() == Some(VarIndex::new(i64::from(msb), i64::from(lsb)))
                });
                Ok((v, if exact { None } else { spec.bits }))
            }
        };
    }
    // fallback for hierarchies where the dotted lookup fails
    if let Some(v) = h.lookup_var(&path, last.clone()) {
        return Ok((v, spec.bits));
    }
    let suggest = suggestions(h, &spec.name);
    if suggest.is_empty() {
        bail!("no signal \"{}\"", spec.name);
    }
    bail!(
        "no signal \"{}\". did you mean: {}",
        spec.name,
        suggest.join(", ")
    )
}

pub fn iter_vars(h: &Hierarchy) -> impl Iterator<Item = VarRef> + '_ {
    h.all_vars()
}

/// Glob filter used by `--match` and `[types]` keys.
pub fn matches_glob(pattern: &str, name: &str) -> Result<bool> {
    let pat = glob::Pattern::new(pattern)
        .map_err(|e| anyhow::anyhow!("invalid pattern \"{pattern}\": {e}"))?;
    Ok(pat.matches(name))
}

pub fn matches_any(patterns: &[String], name: &str) -> Result<bool> {
    for p in patterns {
        if matches_glob(p, name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// signedness
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignSource {
    Cli,
    Config,
    Inferred,
}

impl SignSource {
    pub fn name(self) -> &'static str {
        match self {
            SignSource::Cli => "cli",
            SignSource::Config => "config",
            SignSource::Inferred => "inferred",
        }
    }
}

/// CLI signedness inputs, already resolved to hierarchy full names.
#[derive(Debug, Clone, Default)]
pub struct SignArgs {
    /// Bare `--signed` / `--unsigned`.
    pub bare: Option<bool>,
    /// `--signed-sig` / `--unsigned-sig`, resolved names.
    pub per_sig: Vec<(String, bool)>,
}

impl SignArgs {
    pub fn resolve(
        h: &Hierarchy,
        signed: bool,
        unsigned: bool,
        signed_sig: &[String],
        unsigned_sig: &[String],
    ) -> Result<SignArgs> {
        if signed && unsigned {
            bail!("--signed and --unsigned are mutually exclusive");
        }
        let bare = if signed {
            Some(true)
        } else if unsigned {
            Some(false)
        } else {
            None
        };
        let mut per_sig = Vec::new();
        for (names, value) in [(signed_sig, true), (unsigned_sig, false)] {
            for name in names {
                let spec = parse_sigspec(name)?;
                let (v, _) = find_var(h, &spec)?;
                per_sig.push((var_full_name(h, v), value));
            }
        }
        Ok(SignArgs { bare, per_sig })
    }

    pub fn lookup(&self, full_name: &str) -> Option<bool> {
        self.per_sig
            .iter()
            .find(|(n, _)| n == full_name)
            .map(|(_, v)| *v)
    }
}

/// Signedness inferred from the dumped type information.
pub fn infer_signed(h: &Hierarchy, v: VarRef) -> bool {
    match h[v].var_type() {
        VarType::Integer
        | VarType::Int
        | VarType::ShortInt
        | VarType::LongInt
        | VarType::Byte
        | VarType::Real
        | VarType::RealTime
        | VarType::ShortReal
        | VarType::RealParameter => true,
        _ => match h[v].vhdl_type_name(h) {
            Some(name) => {
                let name = name.trim().to_lowercase();
                name.starts_with("signed")
                    || name.starts_with("integer")
                    || name.starts_with("natural")
                    || name.starts_with("positive")
            }
            None => false,
        },
    }
}

/// Effective signedness plus where it came from.
pub fn signedness(
    h: &Hierarchy,
    v: VarRef,
    spec: &str,
    full_name: &str,
    sign: &SignArgs,
    types: &TypeTable,
) -> (bool, SignSource) {
    if let Some(value) = sign.lookup(full_name) {
        return (value, SignSource::Cli);
    }
    if let Some(value) = sign.bare {
        return (value, SignSource::Cli);
    }
    if let Some(entry) = types.lookup(spec, full_name)
        && let Some(value) = entry.signed
    {
        return (value, SignSource::Config);
    }
    (infer_signed(h, v), SignSource::Inferred)
}

/// Effective radix: explicit CLI/config `--radix` beats `[types]`.
pub fn radix_for(
    spec: &str,
    full_name: &str,
    cli_default: Option<Radix>,
    types: &TypeTable,
) -> Radix {
    if let Some(r) = cli_default {
        return r;
    }
    types
        .lookup(spec, full_name)
        .and_then(|e| e.radix)
        .unwrap_or(Radix::Auto)
}

// ---------------------------------------------------------------------------
// resolved signal handle
// ---------------------------------------------------------------------------

/// Everything a command needs about one resolved signal.
#[derive(Debug, Clone)]
pub struct SigHandle {
    /// The spec as the user wrote it.
    pub spec: String,
    pub var: VarRef,
    pub signal_ref: SignalRef,
    pub full_name: String,
    pub name: String,
    pub width: u32,
    pub encoding: SignalEncoding,
    pub var_type: &'static str,
    pub direction: &'static str,
    pub index: Option<(i64, i64)>,
    pub slice: Option<(u32, u32)>,
    pub signed: bool,
    pub signed_source: SignSource,
    pub radix: Radix,
}

impl SigHandle {
    /// Width of the value the user asked for (after slicing).
    pub fn eff_width(&self) -> u32 {
        match self.slice {
            Some((msb, lsb)) => msb - lsb + 1,
            None => self.width,
        }
    }

    pub fn is_bit_vector(&self) -> bool {
        matches!(self.encoding, SignalEncoding::BitVector(_))
    }

    pub fn is_real(&self) -> bool {
        matches!(self.encoding, SignalEncoding::Real)
    }

    pub fn is_string(&self) -> bool {
        matches!(self.encoding, SignalEncoding::String)
    }

    /// Apply the requested bit slice to a canonical bit string.
    pub fn apply_slice(&self, bits: &str) -> Result<String> {
        match self.slice {
            Some((msb, lsb)) => slice_bits(bits, msb, lsb),
            None => Ok(bits.to_string()),
        }
    }

    /// Render this signal's canonical bit string.
    pub fn render_bits(&self, bits: &str) -> Result<String> {
        render(bits, self.radix, self.signed)
    }

    pub fn json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("signal".into(), self.full_name.clone().into());
        m.insert("width".into(), self.eff_width().into());
        m.insert("signed".into(), self.signed.into());
        m.insert("radix".into(), self.radix.name().into());
        serde_json::Value::Object(m)
    }
}

/// Context for resolving specs (config + CLI overrides).
pub struct ResolveCtx<'a> {
    pub types: &'a TypeTable,
    pub sign: &'a SignArgs,
    pub radix: Option<Radix>,
}

pub fn var_type_name(t: VarType) -> &'static str {
    match t {
        VarType::Event => "event",
        VarType::Integer => "integer",
        VarType::Parameter => "parameter",
        VarType::Real => "real",
        VarType::Reg => "reg",
        VarType::Supply0 => "supply0",
        VarType::Supply1 => "supply1",
        VarType::Time => "time",
        VarType::Tri => "tri",
        VarType::TriAnd => "triand",
        VarType::TriOr => "trior",
        VarType::TriReg => "trireg",
        VarType::Tri0 => "tri0",
        VarType::Tri1 => "tri1",
        VarType::WAnd => "wand",
        VarType::WOr => "wor",
        VarType::String => "string",
        VarType::Port => "port",
        VarType::SparseArray => "sparsearray",
        VarType::RealTime => "realtime",
        VarType::RealParameter => "realparameter",
        VarType::Bit => "bit",
        VarType::Logic => "logic",
        VarType::Int => "int",
        VarType::ShortInt => "shortint",
        VarType::LongInt => "longint",
        VarType::Byte => "byte",
        VarType::Enum => "enum",
        VarType::ShortReal => "shortreal",
        VarType::Boolean => "boolean",
        VarType::BitVector => "bitvector",
        VarType::StdLogic => "std_logic",
        VarType::StdLogicVector => "std_logic_vector",
        VarType::StdULogic => "std_ulogic",
        VarType::StdULogicVector => "std_ulogic_vector",
        VarType::EventParameter => "eventparameter",
        _ => "unknown",
    }
}

pub fn direction_name(d: VarDirection) -> &'static str {
    match d {
        VarDirection::Unknown => "unknown",
        VarDirection::Implicit => "implicit",
        VarDirection::Input => "input",
        VarDirection::Output => "output",
        VarDirection::InOut => "inout",
        VarDirection::Buffer => "buffer",
        VarDirection::Linkage => "linkage",
    }
}

/// Resolve a spec into a handle, applying config/CLI signedness and radix.
pub fn resolve_sig(h: &Hierarchy, ctx: &ResolveCtx, spec_str: &str) -> Result<SigHandle> {
    let spec = parse_sigspec(spec_str)?;
    let (var, slice) = find_var(h, &spec)?;
    let v = &h[var];
    let full_name = v.full_name(h);
    let width = v
        .length(h)
        .unwrap_or_else(|| v.signal_encoding(h).length().unwrap_or(0));
    if let Some((msb, lsb)) = slice
        && msb >= width.max(1)
    {
        bail!("bit range [{msb}:{lsb}] is out of range for \"{full_name}\" (width {width})");
    }
    let (signed, signed_source) =
        signedness(h, var, &spec.with_bits(), &full_name, ctx.sign, ctx.types);
    let radix = radix_for(&spec.with_bits(), &full_name, ctx.radix, ctx.types);
    Ok(SigHandle {
        spec: spec_str.to_string(),
        var,
        signal_ref: v.signal_ref(),
        full_name,
        name: v.name(h).to_string(),
        width,
        encoding: v.signal_encoding(h),
        var_type: var_type_name(v.var_type()),
        direction: direction_name(v.direction()),
        index: v.index().map(|i| (i.msb(), i.lsb())),
        slice,
        signed,
        signed_source,
        radix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigspec_bits() {
        assert_eq!(
            parse_sigspec("tb.bus[15:8]").unwrap(),
            SigSpec {
                name: "tb.bus".into(),
                bits: Some((15, 8))
            }
        );
        assert_eq!(
            parse_sigspec("tb.clk").unwrap(),
            SigSpec {
                name: "tb.clk".into(),
                bits: None
            }
        );
        assert_eq!(
            parse_sigspec("tb.bus[7:0]").unwrap().with_bits(),
            "tb.bus[7:0]"
        );
        // VHDL-ish names that merely contain brackets stay verbatim
        assert_eq!(
            parse_sigspec("tb.rec[3].x").unwrap().name,
            "tb.rec[3].x".to_string()
        );
        assert!(parse_sigspec("tb.bus[2:7]").is_err());
        assert!(parse_sigspec("[7:0]").is_err());
    }

    #[test]
    fn globs() {
        assert!(matches_glob("tb_top.q*", "tb_top.q3").unwrap());
        assert!(!matches_glob("tb_top.q*", "tb_top.a").unwrap());
        assert!(matches_glob("*cnt*", "tb.cnt_reg").unwrap());
        assert!(!matches_glob("*cnt*", "tb.counter").unwrap());
        assert!(matches_glob("[", "x").is_err());
    }
}
