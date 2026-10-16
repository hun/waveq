//! Minimal, deterministic VCD writer used by `waveq cut`.
//!
//! The writer emits a complete, self-contained VCD for a tick window
//! `[from_tick, to_tick]` of a loaded waveform:
//!
//! * `$version waveq cut $end` first, then a `$comment` naming the source,
//!   the clock and the window, then `$timescale` (the source timescale, or
//!   `1ns` when the source has none/`unknown`), the `$scope`/`$var` header and
//!   `$enddefinitions`;
//! * `#<from_tick - 1>` followed by `$dumpvars` with every signal's value
//!   *strictly before* the window, then one `#<tick>` group per tick in
//!   `[from_tick, to_tick]` carrying every value that changes there, in
//!   ascending tick order; a cut from the very start of a file has no
//!   pre-window value, so its `#<from_tick>` group itself is the `$dumpvars`
//!   block;
//!
//! The initial value deliberately comes from one tick before the window rather
//! than from `from_tick` itself: a change exactly at `from_tick` (the clock
//! edge on the window boundary) must stay a *transition* in the file, otherwise
//! the cut loses that edge and every cycle of the cut is shifted by one. Cycles
//! of a cut therefore start at the window's first edge: cycle `i` of a cut of
//! source cycles `A..B` is source cycle `A + i`.
//!
//! No `$date` is written: two cuts of the same window are byte-identical, which
//! is what `tests/cut.rs` checks and what makes a cut committable as a
//! regression fixture. Nothing is rounded or clamped; the caller passes real
//! ticks.
//!
//! The caller must have loaded the selected signals into `wf`
//! (`Waveform::load_signals`); this module never mutates the waveform.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use wellen::simple::Waveform;
use wellen::{Hierarchy, SignalEncoding, SignalRef, SignalValueRef, Timescale, VarRef, VarType};

use crate::format::{format_time, timscale_unit_name};
use crate::sample::format_real;

/// What a cut produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutStats {
    /// Number of signals declared in the cut.
    pub signals: usize,
    /// Number of value records written: the initial values in `$dumpvars` plus
    /// every later change inside the window.
    pub changes: usize,
    /// First tick of the window (inclusive).
    pub from_tick: u64,
    /// Last tick of the window (inclusive).
    pub to_tick: u64,
}

/// Extra context folded into the `$comment` header line, so a cut carries the
/// provenance of the window it came from. Every field is optional.
#[derive(Debug, Clone, Default)]
pub struct CutComment<'a> {
    /// Source dump path.
    pub source: Option<&'a str>,
    /// Resolved clock signal, when the cut was made in cycle space.
    pub clock: Option<&'a str>,
    /// First cycle of the window (`None` for a whole-file/unknown-clock cut).
    pub from_cycle: Option<i64>,
    /// Last cycle of the window.
    pub to_cycle: Option<i64>,
}

/// Write `vars` over `[from_tick, to_tick]` as VCD to `out` (`None` = stdout).
///
/// `vars` must already be loaded into `wf`. Returns what was written; an empty
/// window (no value records at all) is an error, not an empty file, and the
/// output path is only created once the cut is known to be non-empty.
pub fn write_vcd(
    out: Option<&Path>,
    hierarchy: &Hierarchy,
    wf: &Waveform,
    vars: &[VarRef],
    from_tick: u64,
    to_tick: u64,
) -> Result<CutStats> {
    write_vcd_with_comment(
        out,
        hierarchy,
        wf,
        vars,
        from_tick,
        to_tick,
        &CutComment::default(),
    )
}

/// [`write_vcd`] with provenance for the `$comment` line.
pub fn write_vcd_with_comment(
    out: Option<&Path>,
    hierarchy: &Hierarchy,
    wf: &Waveform,
    vars: &[VarRef],
    from_tick: u64,
    to_tick: u64,
    comment: &CutComment<'_>,
) -> Result<CutStats> {
    if from_tick > to_tick {
        bail!("empty window: from tick {from_tick} is after to tick {to_tick}");
    }
    let ts = hierarchy.timescale();
    let out_vars: Vec<OutVar> = vars
        .iter()
        .enumerate()
        .map(|(i, vr)| OutVar::new(hierarchy, *vr, i))
        .collect();

    // Collect every value record first: a failing/empty cut must not truncate
    // an existing output file.
    //
    // Each signal's *initial* value is the one strictly before the window
    // (`from_tick - 1`), and every change with `from_tick <= tick <= to_tick` is
    // written as a normal `#tick` group — including a change exactly at
    // `from_tick`. Writing the settled value at `from_tick` as the initial value
    // instead would silently swallow that change and therefore drop the clock
    // edge on the window boundary, shifting every cycle of the cut by one.
    let pad_tick = from_tick.checked_sub(1);
    let mut initial: Vec<String> = Vec::new();
    let mut groups: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for var in &out_vars {
        let Some(sig) = wf.get_signal(var.signal_ref) else {
            continue;
        };
        // last change strictly before the window start
        let mut pre: Option<(u64, SignalValueRef<'_>)> = None;
        for (idx, value) in sig.iter_changes() {
            let tick = wf.time_table()[idx as usize];
            if tick < from_tick {
                if pre.as_ref().is_none_or(|(best, _)| tick >= *best) {
                    pre = Some((tick, value));
                }
                continue;
            }
            if tick > to_tick {
                continue;
            }
            if let Some(line) = value_line(var, value) {
                groups.entry(tick).or_default().push(line);
            }
        }
        if pad_tick.is_some()
            && let Some((_, value)) = pre
            && let Some(line) = value_line(var, value)
        {
            initial.push(line);
        }
    }
    let changes = initial.len() + groups.values().map(Vec::len).sum::<usize>();
    if changes == 0 {
        bail!(
            "empty cut: none of the {} selected signals has a value in ticks {}..{} ({}..{})",
            out_vars.len(),
            from_tick,
            to_tick,
            format_time(from_tick, ts),
            format_time(to_tick, ts)
        );
    }

    let mut sink: Box<dyn Write> = match out {
        Some(path) => {
            Box::new(BufWriter::new(File::create(path).with_context(|| {
                format!("cannot create cut file {}", path.display())
            })?))
        }
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    writeln!(sink, "$version waveq cut $end")?;
    writeln!(sink, "$comment {} $end", comment_text(comment))?;
    writeln!(sink, "$timescale {} $end", timescale_token(ts))?;
    let tree = scope_tree(&out_vars);
    for child in &tree.children {
        write_scope(&mut sink, child, &out_vars)?;
    }
    writeln!(sink, "$enddefinitions $end")?;

    // The initial block sits one tick before the window so that a change at
    // `from_tick` stays a transition (and the clock edge on the boundary is
    // visible). When no signal has a pre-window value (a cut from the start of
    // the file has none), the window's first tick *is* the initial state and it
    // becomes the `$dumpvars` block instead.
    let mut dumped_first = false;
    if let Some(pad) = pad_tick
        && !initial.is_empty()
    {
        writeln!(sink, "#{pad}")?;
        writeln!(sink, "$dumpvars")?;
        for line in &initial {
            writeln!(sink, "{line}")?;
        }
        writeln!(sink, "$end")?;
        dumped_first = true;
    }
    for (tick, lines) in &groups {
        writeln!(sink, "#{tick}")?;
        let dumpvars = !dumped_first && *tick == from_tick;
        if dumpvars {
            writeln!(sink, "$dumpvars")?;
            dumped_first = true;
        }
        for line in lines {
            writeln!(sink, "{line}")?;
        }
        if dumpvars {
            writeln!(sink, "$end")?;
        }
    }
    sink.flush().context("cannot flush the cut VCD")?;

    Ok(CutStats {
        signals: out_vars.len(),
        changes,
        from_tick,
        to_tick,
    })
}

// ---------------------------------------------------------------------------
// header helpers
// ---------------------------------------------------------------------------

/// `$timescale` token: `factor + unit`, never `unknown`; absent/unknown
/// timescales become `1ns`.
fn timescale_token(ts: Option<Timescale>) -> String {
    match ts {
        Some(t) if t.factor > 0 && t.unit.to_exponent().is_some() => {
            format!("{}{}", t.factor, timscale_unit_name(t.unit))
        }
        _ => "1ns".to_string(),
    }
}

fn comment_text(c: &CutComment<'_>) -> String {
    let mut parts = vec!["waveq cut".to_string()];
    if let Some(source) = c.source {
        parts.push(format!("source {source}"));
    }
    if let Some(clock) = c.clock {
        parts.push(format!("clock {clock}"));
    }
    match (c.from_cycle, c.to_cycle) {
        (Some(a), Some(b)) => parts.push(format!("cycles {a}..{b}")),
        (Some(a), None) => parts.push(format!("from cycle {a}")),
        (None, Some(b)) => parts.push(format!("to cycle {b}")),
        (None, None) => {}
    }
    // a `$comment` body must stay on one line and must not contain `$end`
    parts
        .join(", ")
        .replace(['\n', '\r'], " ")
        .replace("$end", "")
}

// ---------------------------------------------------------------------------
// variable table and scope tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A bit-vector (2/4/9-state) signal of the given width.
    Bits,
    /// A zero-width event signal, written as a 1-bit `event`.
    Event,
    Real,
    Str,
}

struct OutVar {
    signal_ref: SignalRef,
    leaf: String,
    /// Hierarchy path of scopes containing the variable.
    path: Vec<String>,
    id: String,
    vcd_type: &'static str,
    /// The width written in the `$var` line.
    width: u32,
}

impl OutVar {
    fn new(h: &Hierarchy, vr: VarRef, index: usize) -> OutVar {
        let v = &h[vr];
        let full_name = v.full_name(h);
        let leaf = v.name(h).to_string();
        let (kind, width) = match v.signal_encoding(h) {
            SignalEncoding::BitVector(0) => (Kind::Event, 1),
            SignalEncoding::BitVector(n) => (Kind::Bits, n),
            SignalEncoding::Real => (Kind::Real, 1),
            SignalEncoding::String => (Kind::Str, 1),
        };
        OutVar {
            signal_ref: v.signal_ref(),
            path: scope_path(&full_name, &leaf),
            leaf,
            id: id_code(index),
            vcd_type: vcd_type(kind, v.var_type()),
            width,
        }
    }
}

/// Scope segments of `full_name` with `leaf` (the variable name) removed.
fn scope_path(full_name: &str, leaf: &str) -> Vec<String> {
    match full_name.strip_suffix(leaf) {
        Some(prefix) if prefix.ends_with('.') => prefix[..prefix.len() - 1]
            .split('.')
            .map(|s| {
                if s.is_empty() {
                    "unnamed".to_string()
                } else {
                    s.to_string()
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// VCD keyword for a `$var` line.
///
/// The keyword has to agree with the *encoding* (wellen derives
/// `SignalEncoding` from the VCD type, so a `real`-encoded signal must be
/// declared `real`, and so on) while being as close to the source HDL type as
/// the VCD keyword set allows. VHDL/GHW types have no VCD keyword and fall
/// back to `logic`/`wire`, which still round-trips the encoding.
fn vcd_type(kind: Kind, var_type: VarType) -> &'static str {
    match kind {
        Kind::Event => "event",
        Kind::Str => "string",
        Kind::Real => match var_type {
            VarType::RealTime => "realtime",
            VarType::ShortReal => "shortreal",
            VarType::RealParameter => "real_parameter",
            _ => "real",
        },
        Kind::Bits => match var_type {
            VarType::Reg => "reg",
            VarType::Integer => "integer",
            VarType::Parameter => "parameter",
            VarType::Time => "time",
            VarType::Bit => "bit",
            VarType::Logic => "logic",
            VarType::Int => "int",
            VarType::ShortInt => "shortint",
            VarType::LongInt => "longint",
            VarType::Byte => "byte",
            VarType::Enum => "enum",
            VarType::Supply0 => "supply0",
            VarType::Supply1 => "supply1",
            VarType::Tri => "tri",
            VarType::TriAnd => "triand",
            VarType::TriOr => "trior",
            VarType::TriReg => "trireg",
            VarType::Tri0 => "tri0",
            VarType::Tri1 => "tri1",
            VarType::WAnd => "wand",
            VarType::WOr => "wor",
            VarType::SparseArray => "sparray",
            VarType::Port => "port",
            _ => "wire",
        },
    }
}

/// Deterministic identifier code: `index` in base 52 over `a..zA..Z`, written
/// least-significant-first so the first character is always a letter (the
/// scalar value form is `<bit><id>` with no separator).
fn id_code(mut index: usize) -> String {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut out = String::new();
    loop {
        out.push(LETTERS[index % LETTERS.len()] as char);
        index /= LETTERS.len();
        if index == 0 {
            return out;
        }
    }
}

#[derive(Default)]
struct ScopeNode {
    name: String,
    vars: Vec<usize>,
    children: Vec<ScopeNode>,
}

impl ScopeNode {
    fn child(&mut self, name: &str) -> &mut ScopeNode {
        if let Some(pos) = self.children.iter().position(|c| c.name == name) {
            return &mut self.children[pos];
        }
        self.children.push(ScopeNode {
            name: name.to_string(),
            ..ScopeNode::default()
        });
        self.children.last_mut().expect("just pushed")
    }
}

/// Build the scope tree of the selected variables. Variables without a
/// hierarchy path (top-level variables) go into a synthetic `top` scope,
/// because VCD requires every `$var` to live inside a scope.
fn scope_tree(vars: &[OutVar]) -> ScopeNode {
    fn insert(node: &mut ScopeNode, path: &[String], index: usize) {
        match path.split_first() {
            None => node.vars.push(index),
            Some((head, rest)) => insert(node.child(head), rest, index),
        }
    }
    let mut root = ScopeNode::default();
    for (i, var) in vars.iter().enumerate() {
        match var.path.split_first() {
            Some(_) => insert(&mut root, &var.path, i),
            None => insert(&mut root, &["top".to_string()], i),
        }
    }
    root
}

fn write_scope(w: &mut impl Write, node: &ScopeNode, vars: &[OutVar]) -> Result<()> {
    writeln!(w, "$scope module {} $end", node.name)?;
    for &i in &node.vars {
        let v = &vars[i];
        writeln!(
            w,
            "$var {} {} {} {} $end",
            v.vcd_type, v.width, v.id, v.leaf
        )?;
    }
    for child in &node.children {
        write_scope(w, child, vars)?;
    }
    writeln!(w, "$upscope $end")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// values
// ---------------------------------------------------------------------------

/// One VCD value record.
fn value_line(var: &OutVar, value: SignalValueRef<'_>) -> Option<String> {
    match value {
        SignalValueRef::BitVec(bv) => {
            let bits = fit_bits(&bv.bit_string(), var.width);
            if bits.len() <= 1 {
                Some(format!("{bits}{}", var.id))
            } else {
                Some(format!("b{bits} {}", var.id))
            }
        }
        SignalValueRef::Real(r) => Some(format!("r{} {}", format_real(r), var.id)),
        SignalValueRef::String(s) => Some(format!("s{} {}", escape_string(s), var.id)),
        SignalValueRef::Event => Some(format!("1{}", var.id)),
    }
}

/// Force a value to the width declared in `$var`: right-align it and extend
/// with `0` (or with the leading `x`/`z`), the same rule wellen applies when it
/// reads a short value back.
fn fit_bits(bits: &str, width: u32) -> String {
    let width = width as usize;
    let cur = bits.chars().count();
    if cur == width {
        return bits.to_string();
    }
    if cur > width {
        return bits.chars().skip(cur - width).collect();
    }
    let pad = bits
        .chars()
        .next()
        .filter(|c| matches!(c, 'x' | 'z' | 'X' | 'Z'))
        .unwrap_or('0');
    let mut out = String::with_capacity(width);
    for _ in 0..(width - cur) {
        out.push(pad);
    }
    out.push_str(bits);
    out
}

/// VCD value tokens are whitespace-delimited, so a string value has to escape
/// whitespace and backslashes as `\xHH` (what wellen and libfst unescape).
fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_whitespace() || ch == '\\' {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("\\x{b:02x}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load;

    const VCD: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/data/pipeline.vcd"
    );

    #[test]
    fn ids_are_unique_letters_first() {
        let ids: Vec<String> = (0..140).map(id_code).collect();
        for id in &ids {
            assert!(id.chars().next().unwrap().is_ascii_alphabetic());
        }
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "id codes must be unique");
        assert_eq!(id_code(0), "a");
        assert_eq!(id_code(1), "b");
        assert_eq!(id_code(51), "Z");
        assert_eq!(id_code(52), "ab");
    }

    #[test]
    fn bits_are_fitted_to_the_declared_width() {
        assert_eq!(fit_bits("101", 3), "101");
        assert_eq!(fit_bits("101", 5), "00101");
        assert_eq!(fit_bits("1101", 3), "101");
        assert_eq!(fit_bits("xz", 4), "xxxz");
        assert_eq!(fit_bits("z", 3), "zzz");
    }

    #[test]
    fn timescale_never_unknown() {
        use wellen::TimescaleUnit;
        assert_eq!(timescale_token(None), "1ns");
        assert_eq!(
            timescale_token(Some(Timescale::new(1, TimescaleUnit::PicoSeconds))),
            "1ps"
        );
        assert_eq!(
            timescale_token(Some(Timescale::new(10, TimescaleUnit::NanoSeconds))),
            "10ns"
        );
        assert_eq!(
            timescale_token(Some(Timescale::new(0, TimescaleUnit::Seconds))),
            "1ns"
        );
        assert_eq!(
            timescale_token(Some(Timescale::new(1, TimescaleUnit::Unknown))),
            "1ns"
        );
    }

    #[test]
    fn strings_escape_whitespace_and_backslash() {
        assert_eq!(escape_string("abc"), "abc");
        assert_eq!(escape_string("a b"), "a\\x20b");
        assert_eq!(escape_string("a\\b"), "a\\x5cb");
        assert_eq!(escape_string(""), "");
    }

    /// The writer's output must be readable by waveq itself, with the same
    /// values the source dump has over the window.
    #[test]
    fn cut_round_trips_through_wellen() {
        let opts = load::load_options();
        let mut src = load::full(Path::new(VCD), &opts).unwrap();
        let (vars, from_tick, to_tick) = {
            let h = src.hierarchy();
            let names = ["tb_top.ref_y", "tb_top.q3", "tb_top.clk"];
            let vars: Vec<VarRef> = names
                .iter()
                .map(|n| match h.lookup_item_by_name(n) {
                    Some(wellen::ItemRef::Var(v)) => v,
                    other => panic!("{n} not found: {other:?}"),
                })
                .collect();
            let tt = src.time_table().to_vec();
            (vars, tt[0], tt[tt.len() - 1])
        };
        let refs: Vec<SignalRef> = {
            let h = src.hierarchy();
            vars.iter().map(|v| h[*v].signal_ref()).collect()
        };
        src.load_signals_multi_threaded(&refs);

        let out = std::env::temp_dir().join(format!("waveq_vcd_rt_{}.vcd", std::process::id()));
        let stats = {
            let h = src.hierarchy();
            write_vcd_with_comment(
                Some(&out),
                h,
                &src,
                &vars,
                from_tick,
                to_tick,
                &CutComment {
                    source: Some(VCD),
                    clock: Some("tb_top.clk"),
                    from_cycle: Some(0),
                    to_cycle: Some(19),
                },
            )
            .unwrap()
        };
        assert_eq!(stats.signals, 3);
        assert!(stats.changes > 3, "changes: {}", stats.changes);

        let bytes = std::fs::read(&out).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("$version waveq cut $end\n"), "{text:.200}");
        assert!(text.contains("$timescale 1ps $end"), "{text:.400}");
        assert!(text.contains("$scope module tb_top $end"));
        assert!(!text.contains("$date"));

        // the cut reads back with the same timescale and the same values
        let mut cut = load::full(&out, &opts).unwrap();
        assert_eq!(cut.hierarchy().timescale(), src.hierarchy().timescale());
        let cut_vars: Vec<VarRef> = {
            let h = cut.hierarchy();
            ["tb_top.ref_y", "tb_top.q3", "tb_top.clk"]
                .iter()
                .map(|n| match h.lookup_item_by_name(n) {
                    Some(wellen::ItemRef::Var(v)) => v,
                    other => panic!("{n} missing in the cut: {other:?}"),
                })
                .collect()
        };
        let cut_refs: Vec<SignalRef> = {
            let h = cut.hierarchy();
            cut_vars.iter().map(|v| h[*v].signal_ref()).collect()
        };
        cut.load_signals_multi_threaded(&cut_refs);

        for tick in [from_tick, from_tick + 1, to_tick / 2, to_tick] {
            for (a, b) in vars.iter().zip(cut_vars.iter()) {
                let x = {
                    let h = src.hierarchy();
                    crate::sample::sample_at_tick(
                        &src,
                        src.get_signal(h[*a].signal_ref()).unwrap(),
                        tick,
                    )
                    .and_then(|s| s.bits)
                };
                let y = {
                    let h = cut.hierarchy();
                    crate::sample::sample_at_tick(
                        &cut,
                        cut.get_signal(h[*b].signal_ref()).unwrap(),
                        tick,
                    )
                    .and_then(|s| s.bits)
                };
                assert_eq!(x, y, "tick {tick}");
            }
        }
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn real_and_string_values_round_trip() {
        // a dump with one real and one string variable; the string contains a
        // space, which a VCD value token cannot carry literally
        let src_path =
            std::env::temp_dir().join(format!("waveq_vcd_rs_{}.vcd", std::process::id()));
        std::fs::write(
            &src_path,
            "$timescale 1ns $end\n\
             $scope module t $end\n\
             $var real 1 ! r $end\n\
             $var string 1 \" s $end\n\
             $upscope $end\n\
             $enddefinitions $end\n\
             #10\n\
             r3.5 !\n\
             sa\\x20b \"\n\
             #20\n\
             r-2 !\n",
        )
        .unwrap();
        let opts = load::load_options();
        let mut src = load::full(&src_path, &opts).unwrap();
        let vars: Vec<VarRef> = {
            let h = src.hierarchy();
            ["t.r", "t.s"]
                .iter()
                .map(|n| match h.lookup_item_by_name(n) {
                    Some(wellen::ItemRef::Var(v)) => v,
                    other => panic!("missing {n}: {other:?}"),
                })
                .collect()
        };
        let refs: Vec<SignalRef> = {
            let h = src.hierarchy();
            vars.iter().map(|v| h[*v].signal_ref()).collect()
        };
        src.load_signals_multi_threaded(&refs);

        let out = std::env::temp_dir().join(format!("waveq_vcd_rs_cut_{}.vcd", std::process::id()));
        {
            let h = src.hierarchy();
            write_vcd(Some(&out), h, &src, &vars, 10, 20).unwrap();
        }
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("$var real 1 "), "{text}");
        assert!(text.contains("$var string 1 "), "{text}");
        assert!(text.contains("r3.5 "), "{text}");
        assert!(text.contains(r"sa\x20b "), "{text}");

        let mut cut = load::full(&out, &opts).unwrap();
        let cut_vars: Vec<VarRef> = {
            let h = cut.hierarchy();
            ["t.r", "t.s"]
                .iter()
                .map(|n| match h.lookup_item_by_name(n) {
                    Some(wellen::ItemRef::Var(v)) => v,
                    other => panic!("missing {n} in the cut: {other:?}"),
                })
                .collect()
        };
        let cut_refs: Vec<SignalRef> = {
            let h = cut.hierarchy();
            cut_vars.iter().map(|v| h[*v].signal_ref()).collect()
        };
        cut.load_signals_multi_threaded(&cut_refs);

        let sample = |wf: &Waveform, var: VarRef, tick: u64| {
            let h = wf.hierarchy();
            let sig = wf.get_signal(h[var].signal_ref()).unwrap();
            crate::sample::sample_at_tick(wf, sig, tick)
                .map(|s| crate::sample::Val::from_ref(s.value))
        };
        for tick in [10u64, 15, 20] {
            assert_eq!(
                sample(&src, vars[0], tick),
                sample(&cut, cut_vars[0], tick),
                "real at {tick}"
            );
            assert_eq!(
                sample(&src, vars[1], tick),
                sample(&cut, cut_vars[1], tick),
                "string at {tick}"
            );
        }
        assert_eq!(
            sample(&cut, cut_vars[1], 10),
            Some(crate::sample::Val::Str("a b".to_string()))
        );
        assert_eq!(
            sample(&cut, cut_vars[0], 20),
            Some(crate::sample::Val::Real(-2.0))
        );
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&src_path);
    }

    #[test]
    fn empty_window_is_an_error_and_creates_no_file() {
        // a one-signal dump whose only changes are at ticks 100 and 200
        let src_path =
            std::env::temp_dir().join(format!("waveq_vcd_src_{}.vcd", std::process::id()));
        std::fs::write(
            &src_path,
            "$timescale 1ns $end\n\
             $scope module t $end\n\
             $var wire 1 ! a $end\n\
             $upscope $end\n\
             $enddefinitions $end\n\
             #100\n\
             1!\n\
             #200\n\
             0!\n",
        )
        .unwrap();
        let opts = load::load_options();
        let mut src = load::full(&src_path, &opts).unwrap();
        let vars: Vec<VarRef> = {
            let h = src.hierarchy();
            match h.lookup_item_by_name("t.a") {
                Some(wellen::ItemRef::Var(v)) => vec![v],
                other => panic!("missing t.a: {other:?}"),
            }
        };
        let refs: Vec<SignalRef> = {
            let h = src.hierarchy();
            vars.iter().map(|v| h[*v].signal_ref()).collect()
        };
        src.load_signals_multi_threaded(&refs);
        let out = std::env::temp_dir().join(format!("waveq_vcd_empty_{}.vcd", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let err = {
            let h = src.hierarchy();
            write_vcd(Some(&out), h, &src, &vars, 0, 50).unwrap_err()
        };
        assert!(err.to_string().contains("empty cut"), "{err}");
        assert!(!out.exists(), "a failed cut must not create the file");
        let _ = std::fs::remove_file(&src_path);
    }

    #[test]
    fn byte_reproducible() {
        let opts = load::load_options();
        let mut src = load::full(Path::new(VCD), &opts).unwrap();
        let vars: Vec<VarRef> = {
            let h = src.hierarchy();
            ["tb_top.ref_y", "tb_top.q1", "tb_top.q3", "tb_top.a"]
                .iter()
                .map(|n| match h.lookup_item_by_name(n) {
                    Some(wellen::ItemRef::Var(v)) => v,
                    _ => panic!("{n} missing"),
                })
                .collect()
        };
        let refs: Vec<SignalRef> = {
            let h = src.hierarchy();
            vars.iter().map(|v| h[*v].signal_ref()).collect()
        };
        src.load_signals_multi_threaded(&refs);

        let a = std::env::temp_dir().join(format!("waveq_vcd_a_{}.vcd", std::process::id()));
        let b = std::env::temp_dir().join(format!("waveq_vcd_b_{}.vcd", std::process::id()));
        let h = src.hierarchy();
        write_vcd(Some(&a), h, &src, &vars, 0, 1_000_000).unwrap();
        write_vcd(Some(&b), h, &src, &vars, 0, 1_000_000).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
}
