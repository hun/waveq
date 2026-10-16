//! Tests for `waveq cut`.
//!
//! Two layers:
//!
//! * binary-level tests run the real `waveq` (`env!("CARGO_BIN_EXE_waveq")`),
//!   cut a window of the committed fixture into a temp file, and then query the
//!   *cut* with a second invocation to prove it is a valid, self-contained dump;
//! * library-level tests call `cmd::cut::cut` directly with a `CutArgs`, which
//!   is where the error paths (out-of-range cycles, bit slices, empty windows)
//!   are pinned down without depending on CLI wiring.
//!
//! Cycle numbering of a cut starts at the cut's own first clock edge: a cut of
//! source cycles `A..B` has cycles `0..B-A`, so the source row for cycle `A+i`
//! is the cut row for cycle `i`. Fixture numbers: cycle 13 is the single
//! divergent cycle (`ref_y = 56` vs `q3 = 45`, `q3` lags `ref_y` by 3).

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use waveq::cli::Cli;
use waveq::cmd::App;
use waveq::cmd::cut::{CutArgs, cut};
use waveq::output::Format;

const BIN: &str = env!("CARGO_BIN_EXE_waveq");
const VCD: &str = "tests/fixtures/data/pipeline.vcd";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn tmp_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("waveq_cut_{}_{}_{n}.vcd", std::process::id(), tag))
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
                self.stdout, self.stderr
            )
        })
    }

    fn expect_code(&self, want: i32) -> &Out {
        assert_eq!(
            self.code, want,
            "exit code {want} expected, got {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        );
        self
    }

    fn expect_stderr(&self, needle: &str) -> &Out {
        assert!(
            self.stderr.contains(needle),
            "stderr should contain {needle:?}, got:\n{}",
            self.stderr
        );
        self
    }
}

fn run(args: &[&str]) -> Out {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("failed to spawn waveq");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn waveq(file: &Path, rest: &[&str]) -> Out {
    let mut args = vec!["-f", file.to_str().unwrap(), "--no-config"];
    args.extend_from_slice(rest);
    run(&args)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// (a) a cycle window cut is a self-contained dump with the same values
// ---------------------------------------------------------------------------

#[test]
fn cut_cycle_window_round_trips() {
    let file = tmp_path("window");
    let out = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "11..15",
            "--signal",
            "tb_top.q1,tb_top.q3,tb_top.clk",
            "-o",
            file.to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["from_cycle"], 11);
    assert_eq!(doc["to_cycle"], 15);
    assert_eq!(doc["from_tick"], 115_000);
    assert_eq!(doc["to_tick"], 155_000);
    assert_eq!(doc["from_time"], "115ns");
    assert_eq!(doc["to_time"], "155ns");
    assert_eq!(doc["signals"], 3);
    assert!(doc["changes"].as_u64().unwrap() > 0, "{doc}");
    assert_eq!(doc["output"], file.to_str().unwrap());
    assert_eq!(doc["clock"], "tb_top.clk");
    // the clock was selected explicitly, so nothing had to be added, but the
    // cut re-bases cycles and says how to read them back
    assert!(doc.get("added_clock").is_none(), "{doc}");
    assert!(
        doc["read_with"]
            .as_str()
            .unwrap()
            .contains("--cycle-offset -11"),
        "{doc}"
    );

    // smaller than the source, byte-reproducible, self-describing
    let cut_len = std::fs::metadata(&file).unwrap().len();
    let src_len = std::fs::metadata(VCD).unwrap().len();
    assert!(cut_len < src_len, "cut {cut_len} !< source {src_len}");
    let text = read(&file);
    assert!(text.starts_with("$version waveq cut $end"), "{text:.120}");
    assert!(text.contains("$timescale 1ps $end"), "{text:.400}");
    assert!(!text.contains("$date"), "cut must stay byte-reproducible");
    // the initial block is one tick before the window so that the clock edge at
    // 115000 stays a transition
    assert!(text.contains("#114999\n$dumpvars\n"), "{text:.600}");

    // `info` on the cut: a VCD with exactly the three selected vars, spanning
    // the window (plus the single pre-window tick of the initial block)
    let info = waveq(&file, &["info", "--full", "--format", "json"]);
    info.expect_code(0);
    let info = info.json();
    assert_eq!(info["format"], "vcd");
    assert_eq!(info["version"], "waveq cut");
    assert_eq!(info["vars"], 3);
    assert_eq!(info["first_tick"], 114_999);
    assert_eq!(info["last_tick"], 155_000);

    // `table` on the cut with the source's cycle numbers reproduces the source
    // rows exactly (cycles and values)
    let args = [
        "table",
        "--cycles",
        "11..15",
        "--radix",
        "dec",
        "--signal",
        "tb_top.q1,tb_top.q3,tb_top.clk",
        "--format",
        "json",
    ];
    let src = waveq(Path::new(VCD), &args);
    let mut with_offset = vec!["--cycle-offset", "-11"];
    with_offset.extend_from_slice(&args);
    let cut_tbl = waveq(&file, &with_offset);
    src.expect_code(0);
    cut_tbl.expect_code(0);
    assert_eq!(
        src.json()["rows"],
        cut_tbl.json()["rows"],
        "cycle numbers and values must survive a cut"
    );
    let rows = cut_tbl.json()["rows"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["cycle"], 11 + i as i64);
        assert_eq!(row["values"]["tb_top.clk"]["value"], "1");
    }
    // the divergent cycle is inside the window and keeps its values
    assert_eq!(rows[2]["values"]["tb_top.q3"]["bits"], "000101101");
    assert_eq!(rows[2]["values"]["tb_top.q1"]["bits"], "000110100");

    let _ = std::fs::remove_file(&file);
}

// ---------------------------------------------------------------------------
// the window boundary: the first clock edge must survive the cut
// ---------------------------------------------------------------------------

#[test]
fn cut_window_starts_on_a_clock_edge() {
    // cycles 11..15 of the fixture, without naming the clock: it is added
    // automatically and its phase must survive the window start, otherwise the
    // cut's first edge (and every cycle number) would be one cycle late
    let file = tmp_path("edge");
    let out = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "11..15",
            "--signal",
            "tb_top.q1,tb_top.q3",
            "-o",
            file.to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    out.expect_code(0);
    out.expect_stderr("cut: added the clock signal tb_top.clk to keep cycle numbering");
    assert_eq!(out.json()["added_clock"], "tb_top.clk");
    assert_eq!(out.json()["signals"], 3);

    // exactly the five edges of the window, starting at the source's cycle 11
    let clock = waveq(&file, &["clock", "--format", "json"]);
    clock.expect_code(0);
    let clock = &clock.json()["clock"];
    assert_eq!(clock["signal"], "tb_top.clk");
    assert_eq!(clock["edges"], 5);
    assert_eq!(clock["first_tick"], 115_000);
    assert_eq!(clock["first_time"], "115ns");
    assert_eq!(clock["last_tick"], 155_000);
    assert_eq!(clock["last_time"], "155ns");

    // and the clock is high in every cycle of the cut (post-edge sampling)
    let rows = waveq(
        &file,
        &[
            "table",
            "--cycles",
            "0..4",
            "--signal",
            "tb_top.clk",
            "--format",
            "json",
        ],
    );
    rows.expect_code(0);
    let rows = rows.json()["rows"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["cycle"], i as i64);
        assert_eq!(row["values"]["tb_top.clk"]["value"], "1", "cycle {i}");
        assert_eq!(row["tick"], 115_000 + 10_000 * i as i64);
    }

    let _ = std::fs::remove_file(&file);
}

#[test]
fn cut_compare_on_the_cut_matches_the_source() {
    // cycles 10..14 in the source -> cycles 0..4 in the cut; the corrupted
    // transfer is the pair (a=ref_y[10]=44, b=q3[13]=45) at shift 3
    let file = tmp_path("compare");
    waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "10..14",
            "--signal",
            "tb_top.ref_y,tb_top.q3",
            "-o",
            file.to_str().unwrap(),
        ],
    )
    .expect_code(0);

    // the source: shift 3 shows exactly one divergence, reported in the DUT
    // cycle (the b side)
    let src = waveq(
        Path::new(VCD),
        &[
            "compare",
            "--map",
            "tb_top.ref_y=tb_top.q3:3",
            "--from-cycle",
            "10",
            "--to-cycle",
            "14",
            "--format",
            "json",
        ],
    );
    src.expect_code(1);
    let src_pair = &src.json()["pairs"][0];
    assert_eq!(src_pair["shift"], 3);
    assert_eq!(src_pair["mismatches"], 1);
    assert_eq!(src_pair["first_divergent_cycle"], 13);

    // the cut reproduces that divergence, same count, same offset (13 - 10)
    let cut_cmp = waveq(
        &file,
        &[
            "compare",
            "--map",
            "tb_top.ref_y=tb_top.q3:3",
            "--from-cycle",
            "0",
            "--to-cycle",
            "4",
            "--format",
            "json",
        ],
    );
    cut_cmp.expect_code(1);
    let cut_pair = &cut_cmp.json()["pairs"][0];
    assert_eq!(cut_pair["compared"], src_pair["compared"]);
    assert_eq!(cut_pair["mismatches"], src_pair["mismatches"]);
    // the cut's cycles are the window's own 0..4, i.e. source cycle 10 + local
    assert_eq!(cut_pair["first_divergent_cycle"], 3);
    let rows = cut_cmp.json()["rows"].clone();
    let row = &rows.as_array().unwrap()[0];
    assert_eq!(row["a"]["bits"], "000101100"); // ref_y[10] = 44
    assert_eq!(row["b"]["bits"], "000101101"); // q3[13]  = 45
    assert_eq!(row["a_cycle"], 0);
    assert_eq!(row["b_cycle"], 3);
    // absolute times are unchanged by the cut
    assert_eq!(row["time"], src.json()["rows"][0]["time"]);
    assert_eq!(row["time"], "135ns");

    // the unaligned shift 0 differs on every compared cycle, which is the
    // mistake `align` exists to catch
    let wrong = waveq(
        &file,
        &[
            "compare",
            "--map",
            "tb_top.ref_y=tb_top.q3:0",
            "--from-cycle",
            "0",
            "--to-cycle",
            "2",
            "--format",
            "json",
        ],
    );
    wrong.expect_code(1);
    let wrong_pair = &wrong.json()["pairs"][0];
    assert_eq!(wrong_pair["compared"], 3);
    assert_eq!(wrong_pair["mismatches"], 3);

    let _ = std::fs::remove_file(&file);
}

// ---------------------------------------------------------------------------
// the cut keeps the source cycle frame by carrying the clock
// ---------------------------------------------------------------------------

#[test]
fn cut_adds_the_clock_to_keep_cycle_numbers() {
    // `tb_top.q*` does not match the clock, but a cut without it would not be
    // cycle-addressable: cycle 0 of the cut would be the first edge *in the
    // cut*, not the source's cycle 0
    let file = tmp_path("frame");
    let out = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--signal",
            "tb_top.q*",
            "-o",
            file.to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    out.expect_code(0);
    out.expect_stderr("cut: added the clock signal tb_top.clk to keep cycle numbering");
    let doc = out.json();
    assert_eq!(doc["signals"], 4, "q1, q2, q3 + the added clock");
    assert_eq!(doc["added_clock"], "tb_top.clk");
    // tick 0 is before the first edge, so no start cycle is reported; the
    // numbering still matches the source because the cut starts at tick 0
    assert!(doc["from_cycle"].is_null(), "{doc}");
    assert!(doc.get("read_with").is_none(), "no re-basing: {doc}");

    // the cut resolves the same clock ...
    let clock = waveq(&file, &["clock", "--format", "json"]);
    clock.expect_code(0);
    assert_eq!(clock.json()["signal"], "tb_top.clk");

    // ... and starts with the file's own tick 0 as the initial state
    let text = read(&file);
    assert!(text.contains("#0\n$dumpvars\n"), "{text:.600}");
    assert!(text.contains("$timescale 1ps $end"), "{text:.400}");

    // ... so the source cycles remain addressable and identical
    let src = waveq(
        Path::new(VCD),
        &[
            "table",
            "--cycles",
            "11..15",
            "--radix",
            "dec",
            "--signal",
            "tb_top.q3",
            "--format",
            "json",
        ],
    );
    let cut_tbl = waveq(
        &file,
        &[
            "table",
            "--cycles",
            "11..15",
            "--radix",
            "dec",
            "--signal",
            "tb_top.q3",
            "--format",
            "json",
        ],
    );
    src.expect_code(0);
    cut_tbl.expect_code(0);
    assert_eq!(src.json()["rows"], cut_tbl.json()["rows"]);
    assert_eq!(cut_tbl.json()["rows"][2]["cycle"], 13);
    assert_eq!(
        cut_tbl.json()["rows"][2]["values"]["tb_top.q3"]["bits"],
        "000101101"
    );

    let _ = std::fs::remove_file(&file);
}

// ---------------------------------------------------------------------------
// (b) byte reproducibility
// ---------------------------------------------------------------------------

#[test]
fn cut_is_byte_reproducible() {
    let a = tmp_path("repro_a");
    let b = tmp_path("repro_b");
    let base = [
        "-f",
        VCD,
        "--no-config",
        "cut",
        "--cycles",
        "11..15",
        "--signal",
        "tb_top.ref_y,tb_top.q3,tb_top.clk",
        "-o",
    ];
    for p in [&a, &b] {
        let mut args: Vec<&str> = base.to_vec();
        args.push(p.to_str().unwrap());
        run(&args).expect_code(0);
    }
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

// ---------------------------------------------------------------------------
// (c) glob selection
// ---------------------------------------------------------------------------

#[test]
fn cut_glob_selects_matching_signals() {
    let file = tmp_path("glob");
    let out = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "11..15",
            "--signal",
            "tb_top.q*",
            "-o",
            file.to_str().unwrap(),
            "--format",
            "json",
        ],
    );
    out.expect_code(0);
    assert_eq!(out.json()["signals"], 4, "q1, q2, q3 + the added clock");
    assert_eq!(out.json()["added_clock"], "tb_top.clk");

    let sigs = waveq(&file, &["signals", "--format", "json"]);
    sigs.expect_code(0);
    let mut names: Vec<String> = sigs.json()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["full_name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["tb_top.clk", "tb_top.q1", "tb_top.q2", "tb_top.q3"],
        "the glob plus the clock that keeps the cycle frame"
    );

    // an unmatched glob is an error, not an empty cut
    let missing = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "11..15",
            "--signal",
            "tb_top.nope*",
            "-o",
            file.to_str().unwrap(),
        ],
    );
    missing.expect_code(2).expect_stderr("no signal matches");

    let _ = std::fs::remove_file(&file);
}

// ---------------------------------------------------------------------------
// (d) out-of-range bounds are errors (never a clamped file)
// ---------------------------------------------------------------------------

#[test]
fn cut_out_of_range_cycle_is_an_error() {
    let file = tmp_path("oob");
    let out = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "0..9999",
            "--signal",
            "tb_top.q1",
            "-o",
            file.to_str().unwrap(),
        ],
    );
    out.expect_code(2).expect_stderr("out of range");
    assert!(!file.exists(), "a failed cut must not create a file");

    // the last edge is cycle 45; one past it is rejected
    let edge = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--from-cycle",
            "46",
            "--signal",
            "tb_top.q1",
            "-o",
            file.to_str().unwrap(),
        ],
    );
    edge.expect_code(2).expect_stderr("out of range");

    let _ = std::fs::remove_file(&file);
}

// ---------------------------------------------------------------------------
// (e) stdout mode: only VCD bytes on stdout, summary on stderr
// ---------------------------------------------------------------------------

#[test]
fn cut_without_output_writes_only_vcd() {
    let out = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "11..15",
            "--signal",
            "tb_top.q1,tb_top.q3,tb_top.clk",
        ],
    );
    out.expect_code(0);
    assert!(
        out.stdout.starts_with("$version") || out.stdout.starts_with("$date"),
        "stdout must be VCD: {:.120}",
        out.stdout
    );
    assert!(out.stdout.contains("$enddefinitions $end"));
    assert!(
        !out.stdout.contains("from_tick:"),
        "the summary must not leak onto stdout"
    );
    out.expect_stderr("from_tick:");
    out.expect_stderr("changes:");

    // --format json is still clean VCD on stdout
    let json = waveq(
        Path::new(VCD),
        &[
            "cut",
            "--cycles",
            "11..15",
            "--signal",
            "tb_top.q1",
            "--format",
            "json",
        ],
    );
    json.expect_code(0);
    assert!(json.stdout.starts_with("$version"), "{:.120}", json.stdout);
    assert!(
        !json.stdout.contains("{"),
        "no JSON on stdout in stdout mode"
    );

    // and the VCD on stdout can be written to a file and re-read
    let file = tmp_path("piped");
    std::fs::write(&file, &out.stdout).unwrap();
    let info = waveq(&file, &["info", "--format", "json"]);
    info.expect_code(0);
    assert_eq!(info.json()["vars"], 3);
    let _ = std::fs::remove_file(&file);
}

// ---------------------------------------------------------------------------
// library-level tests (no CLI dispatch involved)
// ---------------------------------------------------------------------------

fn app(file: &str, extra: &[&str]) -> App {
    let mut args = vec!["waveq".to_string(), "-f".to_string(), file.to_string()];
    args.push("--no-config".to_string());
    args.extend(extra.iter().map(|s| s.to_string()));
    args.push("info".to_string());
    let cli = <Cli as clap::Parser>::parse_from(args);
    App::new(cli).expect("app")
}

fn window(signals: &[&str]) -> CutArgs {
    CutArgs {
        cycles: Some("11..15".to_string()),
        from_cycle: None,
        to_cycle: None,
        signal: signals.iter().map(|s| s.to_string()).collect(),
        output: Some(tmp_path("lib")),
        all: false,
    }
}

#[test]
fn lib_cut_doc_and_header() {
    let app = app(VCD, &[]);
    let args = window(&["tb_top.q1", "tb_top.q3", "tb_top.clk"]);
    let file = args.output.clone().unwrap();
    let doc = cut(&app, args).unwrap();
    let text = doc.render(Format::Text);
    assert!(text.contains("from_cycle: 11"), "{text}");
    assert!(text.contains("to_cycle: 15"), "{text}");
    assert!(text.contains("from_tick: 115000"), "{text}");
    assert!(text.contains("from_time: 115ns"), "{text}");
    assert!(text.contains("signals: 3"), "{text}");
    assert!(
        text.contains(&format!("output: {}", file.display())),
        "{text}"
    );
    assert!(!doc.suppress_output, "-o prints the document");
    let text = read(&file);
    assert!(text.contains("$comment waveq cut, source tests/fixtures/data/pipeline.vcd, clock tb_top.clk, cycles 11..15 $end"), "{text:.400}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn lib_cut_all_and_whole_file() {
    let app = app(VCD, &[]);
    let file = tmp_path("all");
    let doc = cut(
        &app,
        CutArgs {
            cycles: None,
            from_cycle: None,
            to_cycle: None,
            signal: vec![],
            output: Some(file.clone()),
            all: true,
        },
    )
    .unwrap();
    let text = doc.render(Format::Text);
    assert!(text.contains("signals: 20"), "{text}"); // pipeline.vcd vars
    // tick 0 is before the first clock edge, so the window has no start cycle
    assert!(text.contains("from_cycle: -"), "{text}");
    assert!(text.contains("to_cycle: 45"), "{text}");
    assert!(text.contains("from_tick: 0"), "{text}");
    assert!(text.contains("to_tick: 460000"), "{text}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn lib_cut_rejects_slices_and_unknown_groups() {
    let app = app(VCD, &[]);
    let mut args = window(&["tb_top.a[3:0]"]);
    args.output = None;
    let err = cut(&app, args).unwrap_err().to_string();
    assert!(err.contains("bit-sliced cuts are not supported"), "{err}");

    let mut args = window(&["@inputs"]);
    args.output = None;
    let err = cut(&app, args).unwrap_err().to_string();
    assert!(err.contains("no signal group"), "{err}");
}

#[test]
fn lib_cut_needs_a_clock_for_cycle_bounds() {
    let app = app(VCD, &["--clock", "none"]);
    let mut args = window(&["tb_top.q1"]);
    args.output = None;
    let err = cut(&app, args).unwrap_err().to_string();
    assert!(err.contains("cycle bounds need a clock"), "{err}");

    // without cycle bounds the same app cuts the whole file
    let file = tmp_path("noclk");
    let doc = cut(
        &app,
        CutArgs {
            cycles: None,
            from_cycle: None,
            to_cycle: None,
            signal: vec!["tb_top.q1".to_string()],
            output: Some(file.clone()),
            all: false,
        },
    )
    .unwrap();
    let text = doc.render(Format::Text);
    assert!(text.contains("clock: -"), "{text}");
    assert!(text.contains("from_cycle: -"), "{text}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn lib_stdout_mode_is_suppressed() {
    let app = app(VCD, &[]);
    let mut args = window(&["tb_top.q1"]);
    args.output = None;
    let doc = cut(&app, args).unwrap();
    assert!(
        doc.suppress_output,
        "stdout mode must suppress the document"
    );
    // the summary is still complete for callers that render it themselves
    assert!(doc.render(Format::Text).contains("changes:"));
}
