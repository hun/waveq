//! End-to-end tests: every test shells out to the real binary and inspects
//! `--format json` output and the process exit code.
//!
//! Fixture numbers (see tests/fixtures/src/pipeline.sv for the RTL):
//!   * cycle c is the posedge at tick 5+10c ns (VCD timescale is 1ps, so tick
//!     135000 = 135ns); sampling is post-edge
//!   * pre-stall (cycles 0..19): a_c = c+1, b_c = 3(c+1), ref_y_c = 4(c+1),
//!     q1_c = ref_y[c-1], q2_c = ref_y[c-2], q3_c = ref_y[c-3]
//!   * the q2 load at cycle 12 is corrupted once, so ref_y[13] = 56 is
//!     compared against q3[13] = 45 -> one divergent cycle/transfer
//!   * the 12-cycle stall (cycles 20..31) freezes inputs, data and valid
//!     together: the cycle-space phase jumps while the pipeline refills (so a
//!     fixed shift 3 mismatches 15 cycles over the whole dump), while the
//!     transfer-indexed comparison stays exact and reports the accepted inputs
//!     that have no output event yet (unpaired == pipeline depth == 3)

use serde_json::Value;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_waveq");
const VCD: &str = "tests/fixtures/data/pipeline.vcd";
const FST: &str = "tests/fixtures/data/pipeline.fst";
const GHW: &str = "tests/fixtures/data/counter.ghw";
const XVCD: &str = "tests/fixtures/data/x_values.vcd";
const CFG: &str = "tests/fixtures/waveq.toml";

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

/// `waveq -f <vcd> --no-config <rest...>` in the hermetic default setup.
fn vcd(rest: &[&str]) -> Out {
    let mut args = vec!["-f", VCD, "--no-config"];
    args.extend_from_slice(rest);
    run(&args)
}

fn table_rows(doc: &Value) -> Vec<Value> {
    doc["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("document has no rows array: {doc}"))
        .clone()
}

// ---------------------------------------------------------------------------
// T1: align, cycles mode
// ---------------------------------------------------------------------------

#[test]
fn t1_align_cycles_pre_stall_window() {
    let out = vcd(&[
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--shifts",
        "0..=8",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["best_shift"], 3);
    assert_eq!(doc["best_mismatches"], 1);
    assert_eq!(doc["ambiguous"], false);
    assert_eq!(doc["align"], "cycles");
    assert_eq!(doc["mode"], "cycles");
    assert_eq!(doc["from_cycle"], 0);
    assert_eq!(doc["to_cycle"], 19);

    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 9, "shifts 0..=8");
    for row in &rows {
        let shift = row["shift"].as_u64().unwrap();
        if shift == 3 {
            // both indices stay inside the window: cycles 0..16
            assert_eq!(row["compared"], 17);
            assert_eq!(row["mismatches"], 1);
            assert_eq!(row["first_divergent_cycle"], 13);
            assert_eq!(row["first_divergent_a_cycle"], 10);
            assert_eq!(row["first_divergent_time"], "135ns");
        } else {
            assert!(
                row["mismatches"].as_u64().unwrap() > 5,
                "shift {shift} should be clearly worse: {row}"
            );
        }
    }
}

#[test]
fn t1_align_cycles_unbounded_stall_breaks_the_fixed_shift() {
    let out = vcd(&[
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--shifts",
        "0..=8",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["best_shift"], 3, "shift 3 is still the best alignment");
    assert!(
        doc["best_mismatches"].as_u64().unwrap() >= 10,
        "the stall must break the fixed shift: {doc}"
    );
    assert_eq!(doc["to_cycle"], 45);
    let rows = table_rows(&doc);
    let shift3 = rows.iter().find(|r| r["shift"] == 3).unwrap();
    assert_eq!(shift3["first_divergent_cycle"], 13);
    assert_eq!(shift3["mismatches"], 15);
    let shift0 = rows.iter().find(|r| r["shift"] == 0).unwrap();
    assert!(shift0["mismatches"].as_u64().unwrap() >= 40);
}

// ---------------------------------------------------------------------------
// T2: compare, cycles mode with auto-shift
// ---------------------------------------------------------------------------

#[test]
fn t2_compare_auto_shift_bounded() {
    let out = vcd(&[
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3",
        "--auto-shift",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    let pair = &doc["pairs"][0];
    assert_eq!(pair["shift"], 3);
    assert_eq!(pair["mismatches"], 1);
    assert_eq!(pair["first_divergent_cycle"], 13);
    assert_eq!(pair["first_divergent_time"], "135ns");
    assert_eq!(pair["status"], "divergent");
    assert_eq!(pair["auto_shift_ambiguous"], false);

    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["cycle"], 13, "reported in the DUT cycle");
    assert_eq!(row["a_cycle"], 10);
    assert_eq!(row["b_cycle"], 13);
    assert_eq!(row["time"], "135ns");
    assert_eq!(row["a"]["bits"], "000101100"); // ref_y[10] = 44
    assert_eq!(row["b"]["bits"], "000101101"); // q3[13]  == 45 (corrupted)
    assert_eq!(row["a"]["value"], "02c");
    assert_eq!(row["b"]["value"], "02d");
    assert_eq!(row["cmp"], "mismatch");
    assert_eq!(row["pre_clock"], false);
}

#[test]
fn t2_compare_without_bounds_is_worse() {
    let out = vcd(&[
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3",
        "--auto-shift",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["shift"], 3);
    assert!(
        doc["pairs"][0]["mismatches"].as_u64().unwrap() >= 10,
        "an unbounded cycle-space comparison must show the stall: {doc}"
    );
    assert_eq!(doc["pairs"][0]["first_divergent_cycle"], 13);
}

#[test]
fn t2_probe_columns_show_the_driving_inputs() {
    let out = vcd(&[
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--probe",
        "tb_top.a,tb_top.b",
        "--first-only",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 1, "--first-only keeps one row");
    let row = &rows[0];
    // probes are sampled at the A-side cycle: a_10 = 11, b_10 = 33 -> ref_y[10] = 44
    assert_eq!(row["probe"]["tb_top.a"]["value"], "0b");
    assert_eq!(row["probe"]["tb_top.b"]["value"], "21");
    assert_eq!(row["a"]["value"], "02c");
}

// ---------------------------------------------------------------------------
// T3: compare, transfers mode
// ---------------------------------------------------------------------------

fn transfers_args(window: bool) -> Vec<&'static str> {
    let mut args = vec![
        "compare",
        "--align",
        "transfers",
        "--in-valid",
        "tb_top.in_valid",
        "--out-valid",
        "tb_top.out_go",
        "--map",
        "tb_top.ref_y=tb_top.q3",
        "--format",
        "json",
    ];
    if window {
        args.extend_from_slice(&["--from-cycle", "0", "--to-cycle", "12"]);
    }
    args
}

#[test]
fn t3_compare_transfers_finds_the_single_divergence() {
    let out = vcd(&transfers_args(false));
    out.expect_code(1);
    let doc = out.json();
    let pair = &doc["pairs"][0];
    assert_eq!(pair["mismatches"], 1);
    assert_eq!(pair["a_events"], 34);
    assert_eq!(pair["b_events"], 31);
    assert_eq!(pair["paired"], 31);
    assert_eq!(pair["unpaired"], 3, "unpaired == pipeline depth");
    assert_eq!(pair["first_divergent_cycle"], 13);
    assert!(
        out.stderr.contains("unpaired=3"),
        "stderr note: {}",
        out.stderr
    );

    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["transfer"], 10);
    assert_eq!(row["a_cycle"], 10);
    assert_eq!(row["b_cycle"], 13);
    assert_eq!(row["a"]["bits"], "000101100");
    assert_eq!(row["b"]["bits"], "000101101");
    assert_eq!(row["time"], "135ns");
}

#[test]
fn t3_unpaired_transfers_alone_do_not_fail_the_run() {
    // Same pair, a window that ends before the injected bug: no divergence at
    // all, but the trailing accepted inputs are still unpaired.
    let out = vcd(&transfers_args(true));
    out.expect_code(0);
    let doc = out.json();
    let pair = &doc["pairs"][0];
    assert_eq!(pair["mismatches"], 0);
    assert_eq!(pair["status"], "ok");
    assert_eq!(pair["a_events"], 13);
    assert_eq!(pair["b_events"], 10);
    assert_eq!(pair["unpaired"], 3);
    assert_eq!(table_rows(&doc).len(), 0);
}

// ---------------------------------------------------------------------------
// T4/T5: changes and table
// ---------------------------------------------------------------------------

#[test]
fn t4_changes_reference_cycles() {
    let out = vcd(&[
        "changes",
        "--signal",
        "tb_top.ref_y",
        "--from-cycle",
        "0",
        "--to-cycle",
        "3",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    let got: Vec<(i64, String)> = rows
        .iter()
        .map(|r| {
            (
                r["cycle"].as_i64().unwrap(),
                r["bits"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (0, "000000100".to_string()), // 4
            (1, "000001000".to_string()), // 8
            (2, "000001100".to_string()), // 12
            (3, "000010000".to_string()), // 16
        ]
    );
    assert_eq!(rows[0]["time"], "5ns");
    assert_eq!(rows[0]["signal"], "tb_top.ref_y");
}

#[test]
fn t5_table_context_around_divergent_cycle() {
    let out = vcd(&[
        "table",
        "--at-cycle",
        "13",
        "--context",
        "2",
        "--signal",
        "tb_top.ref_y,tb_top.q1,tb_top.q2,tb_top.q3",
        "--radix",
        "dec",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["from_cycle"], 11);
    assert_eq!(doc["to_cycle"], 15);
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 5);
    let row13 = rows.iter().find(|r| r["cycle"] == 13).unwrap();
    assert_eq!(row13["values"]["tb_top.ref_y"]["value"], "56");
    assert_eq!(row13["values"]["tb_top.q1"]["value"], "52");
    assert_eq!(row13["values"]["tb_top.q2"]["value"], "48");
    assert_eq!(row13["values"]["tb_top.q3"]["value"], "45");
    assert_eq!(row13["time"], "135ns");
    let row14 = rows.iter().find(|r| r["cycle"] == 14).unwrap();
    assert_eq!(row14["values"]["tb_top.q3"]["value"], "48");
    // one cycle later the DUT has recovered
    assert_eq!(row14["values"]["tb_top.ref_y"]["value"], "60");
    assert_eq!(row13["values"]["tb_top.q3"]["bits"], "000101101");
}

#[test]
fn t5_table_default_cap_is_reported() {
    let out = vcd(&[
        "table",
        "--signal",
        "tb_top.a",
        "--max-rows",
        "3",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["omitted"], 43);
    assert_eq!(table_rows(&doc).len(), 3);
    assert!(out.stderr.contains("truncated"));
}

// ---------------------------------------------------------------------------
// T6: signedness and radix precedence
// ---------------------------------------------------------------------------

fn sn_value(extra: &[&str]) -> String {
    let mut args = vec![
        "value",
        "--at",
        "0",
        "--signal",
        "tb_top.sn",
        "--format",
        "json",
    ];
    args.extend_from_slice(extra);
    let out = vcd(&args);
    out.expect_code(0);
    let doc = out.json();
    doc["rows"][0]["value"]["value"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn t6_signedness_and_radix() {
    assert_eq!(sn_value(&["--radix", "dec"]), "253");
    assert_eq!(
        sn_value(&["--radix", "dec", "--signed-sig", "tb_top.sn"]),
        "-3"
    );
    assert_eq!(sn_value(&["--radix", "hex"]), "fd");
    assert_eq!(sn_value(&["--radix", "hex", "--signed"]), "fd");
    assert_eq!(sn_value(&["--radix", "dec", "--signed"]), "-3");
    // per-signal override beats the bare flag
    assert_eq!(
        sn_value(&["--radix", "dec", "--signed", "--unsigned-sig", "tb_top.sn"]),
        "253"
    );
    // signed rendering of the same bits, always with the canonical bit string
    let out = vcd(&[
        "value",
        "--at",
        "0",
        "--signal",
        "tb_top.sn",
        "--radix",
        "dec",
        "--signed",
        "--format",
        "json",
    ]);
    let doc = out.json();
    assert_eq!(doc["rows"][0]["value"]["bits"], "11111101");
    assert_eq!(doc["rows"][0]["value"]["has_x"], false);
}

#[test]
fn t6_radix_auto_and_bit_slices() {
    let out = vcd(&[
        "value",
        "--at",
        "0",
        "--signal",
        "tb_top.q3[8:4],tb_top.a[3:0],tb_top.clk",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows[0]["signal"], "tb_top.q3");
    assert_eq!(rows[0]["width"], 5);
    assert_eq!(rows[0]["value"]["bits"], "00000");
    assert_eq!(rows[0]["value"]["value"], "00");
    assert_eq!(rows[1]["value"]["bits"], "0000");
    assert_eq!(
        rows[2]["value"]["bits"], "0",
        "auto radix keeps 1-bit signals binary"
    );

    let out = vcd(&[
        "value",
        "--at",
        "0",
        "--signal",
        "tb_top.q3[12:0]",
        "--format",
        "json",
    ]);
    out.expect_code(2).expect_stderr("out of range");
}

// ---------------------------------------------------------------------------
// T7: FST parity
// ---------------------------------------------------------------------------

#[test]
fn t7_fst_matches_vcd() {
    let vcd_out = vcd(&[
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--shifts",
        "0..=8",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    let fst_out = run(&[
        "-f",
        FST,
        "--no-config",
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--shifts",
        "0..=8",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    fst_out.expect_code(1);
    let a = vcd_out.json();
    let b = fst_out.json();
    for key in [
        "best_shift",
        "best_mismatches",
        "ambiguous",
        "compared",
        "skipped",
    ] {
        assert_eq!(a[key], b[key], "mismatch on {key}");
    }
    assert_eq!(a["rows"], b["rows"]);
    assert_eq!(b["best_shift"], 3);
    assert_eq!(b["best_mismatches"], 1);
}

// ---------------------------------------------------------------------------
// T8: GHW
// ---------------------------------------------------------------------------

#[test]
fn t8_ghw_info_signals_and_values() {
    let out = run(&["-f", GHW, "--no-config", "info", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["format"], "ghw");
    assert_eq!(doc["timescale"]["factor"], 1);
    assert_eq!(doc["timescale"]["unit"], "fs");

    let out = run(&[
        "-f",
        GHW,
        "--no-config",
        "signals",
        "--match",
        "*cnt*",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["width"], 8);
    assert_eq!(rows[0]["full_name"], "counter_tb.cnt");

    let name = rows[0]["full_name"].as_str().unwrap().to_string();
    let out = run(&[
        "-f",
        GHW,
        "--no-config",
        "value",
        "--at",
        "25ns",
        "--signal",
        &name,
        "--radix",
        "dec",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["rows"][0]["value"]["value"], "2");
    assert_eq!(doc["cycle"], 2, "the posedge at 25ns is cycle 2");
    assert_eq!(doc["tick"], 25000000);
    assert_eq!(doc["time"], "25ns");

    let out = run(&[
        "-f",
        GHW,
        "--no-config",
        "changes",
        "--signal",
        "counter_tb.sn",
        "--radix",
        "dec",
        "--signed",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(table_rows(&doc)[0]["value"], "-3");
}

// ---------------------------------------------------------------------------
// T9: config file
// ---------------------------------------------------------------------------

#[test]
fn t9_config_drives_the_same_code_paths() {
    // named check == the T2 invocation
    let out = run(&[
        "--config",
        CFG,
        "compare",
        "--check",
        "out_delay",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["shift"], 3);
    assert_eq!(doc["pairs"][0]["mismatches"], 1);
    assert_eq!(doc["pairs"][0]["first_divergent_cycle"], 13);
    assert!(out.stderr.contains("using config"));

    // [types] provides signed + radix without any CLI flag
    let out = run(&[
        "--config",
        CFG,
        "value",
        "--at",
        "0",
        "--signal",
        "tb_top.sn",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["rows"][0]["value"]["value"], "-3");
    assert_eq!(doc["rows"][0]["signed"], true);
    assert_eq!(doc["rows"][0]["radix"], "dec");

    // [signals] group expansion
    let out = run(&[
        "--config",
        CFG,
        "table",
        "--at-cycle",
        "13",
        "--context",
        "1",
        "--signal",
        "@inputs",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["from_cycle"], 12);
    assert_eq!(doc["to_cycle"], 14);
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 3);
    assert!(rows[0]["values"]["tb_top.in_valid"].is_object());

    // CLI overrides a named check's fields
    let out = run(&[
        "--config",
        CFG,
        "compare",
        "--check",
        "out_delay",
        "--shift",
        "4",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    assert_eq!(out.json()["pairs"][0]["shift"], 4);

    let out = run(&[
        "--config",
        CFG,
        "compare",
        "--check",
        "out_delay",
        "--from-cycle",
        "0",
        "--to-cycle",
        "5",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["compared"], 3);
    assert_eq!(doc["pairs"][0]["mismatches"], 0);

    // resolved signal table
    let out = run(&["--config", CFG, "config", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    let resolved = doc["resolved"].as_array().expect("resolved is an array");
    let find = |key: &str| -> &Value {
        resolved
            .iter()
            .find(|r| r["key"] == key)
            .unwrap_or_else(|| panic!("no resolved entry {key}: {resolved:?}"))
    };
    assert_eq!(find("types.tb_top.sn")["full_name"], "tb_top.sn");
    assert_eq!(find("types.tb_top.sn")["signed"], true);
    assert_eq!(find("types.tb_top.sn")["radix"], "dec");
    assert_eq!(find("types.tb_top.sn")["signed_source"], "config");
    assert_eq!(find("wave.clock")["full_name"], "tb_top.clk");
    assert_eq!(find("checks.out_delay.a")["full_name"], "tb_top.ref_y");
    assert_eq!(find("checks.out_delay.b")["full_name"], "tb_top.q3");
    assert_eq!(find("signals.inputs[0]")["full_name"], "tb_top.a");
    assert_eq!(doc["config"], CFG);
    assert_eq!(doc["file"], VCD);
}

#[test]
fn t9_no_config_and_precedence() {
    // --no-config: [types] must not apply
    let out = run(&[
        "--no-config",
        "-f",
        VCD,
        "value",
        "--at",
        "0",
        "--signal",
        "tb_top.sn",
        "--radix",
        "dec",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    assert_eq!(out.json()["rows"][0]["value"]["value"], "253");

    // explicit -f beats wave.file
    let out = run(&["--config", CFG, "-f", FST, "info", "--format", "json"]);
    out.expect_code(0);
    assert_eq!(out.json()["format"], "fst");

    // --config and --no-config are mutually exclusive
    let out = run(&["--config", CFG, "--no-config", "info"]);
    out.expect_code(2).expect_stderr("mutually exclusive");

    // unknown key
    let path = std::env::temp_dir().join("waveq_unknown_key.toml");
    std::fs::write(&path, "[wave]\nbogus = 1\n").unwrap();
    let out = run(&["--config", path.to_str().unwrap(), "info"]);
    out.expect_code(2).expect_stderr("unknown config key");
    std::fs::remove_file(&path).ok();

    // unknown key inside a section entry
    let path = std::env::temp_dir().join("waveq_unknown_entry.toml");
    std::fs::write(&path, "[checks]\nx = { a = \"a\", b = \"b\", nope = 1 }\n").unwrap();
    let out = run(&["--config", path.to_str().unwrap(), "info"]);
    out.expect_code(2).expect_stderr("unknown config key");
    std::fs::remove_file(&path).ok();

    // unknown group / unknown check / unknown query all fail loudly
    let out = run(&["-f", VCD, "--no-config", "table", "--signal", "@nope"]);
    out.expect_code(2).expect_stderr("no signal group");
    let out = run(&["-f", VCD, "--no-config", "compare", "--check", "nope"]);
    out.expect_code(2).expect_stderr("no check \"nope\"");
    let out = run(&["-f", VCD, "--no-config", "table", "--query", "nope"]);
    out.expect_code(2).expect_stderr("no query \"nope\"");
}

#[test]
fn t9_config_discovery_stops_at_git_root() {
    // layout:
    //   <tmp>/                  .waveq.toml  (types: sn signed+dec)
    //   <tmp>/project/.git/     -> upward search must stop here
    //   <tmp>/project/sub/      (cwd for the run)
    let root = std::env::temp_dir().join(format!("waveq_discovery_{}", std::process::id()));
    let project = root.join("project");
    let sub = project.join("sub");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(
        root.join(".waveq.toml"),
        "[types]\n\"tb_top.sn\" = { signed = true, radix = \"dec\" }\n",
    )
    .unwrap();

    let vcd_abs = std::fs::canonicalize(VCD).unwrap();

    // from <tmp>/project/sub the search hits project/.git first and stops
    let out = Command::new(BIN)
        .current_dir(&sub)
        .args([
            "-f",
            vcd_abs.to_str().unwrap(),
            "value",
            "--at",
            "0",
            "--signal",
            "tb_top.sn",
            "--radix",
            "dec",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        doc["rows"][0]["value"]["value"], "253",
        "the search must not look above a directory containing .git"
    );

    // from <tmp> itself the .waveq.toml is found and applied
    let out = Command::new(BIN)
        .current_dir(&root)
        .args([
            "-f",
            vcd_abs.to_str().unwrap(),
            "value",
            "--at",
            "0",
            "--signal",
            "tb_top.sn",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0));
    assert!(stderr.contains("using config"), "stderr: {stderr}");
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["rows"][0]["value"]["value"], "-3");
    assert_eq!(doc["rows"][0]["radix"], "dec");

    // --no-config disables it again
    let out = Command::new(BIN)
        .current_dir(&root)
        .args([
            "-f",
            vcd_abs.to_str().unwrap(),
            "--no-config",
            "value",
            "--at",
            "0",
            "--signal",
            "tb_top.sn",
            "--radix",
            "dec",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["rows"][0]["value"]["value"], "253");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn t9_named_query() {
    // the query supplies signals + context; without --at-cycle the whole dump is
    // used (46 cycles), and the CLI-supplied --at-cycle activates context 3
    let out = run(&[
        "--config", CFG, "table", "--query", "pipeline", "--format", "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 46);
    assert_eq!(doc["from_cycle"], 0);
    assert_eq!(doc["to_cycle"], 45);
    assert!(rows[0]["values"]["tb_top.q1"].is_object());
    assert!(rows[0]["values"]["tb_top.q2"].is_object());

    let out = run(&[
        "--config",
        CFG,
        "table",
        "--query",
        "pipeline",
        "--at-cycle",
        "13",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["from_cycle"], 10);
    assert_eq!(doc["to_cycle"], 16);
    assert_eq!(table_rows(&doc).len(), 7);

    // CLI --signal overrides the query's signal list
    let out = run(&[
        "--config",
        CFG,
        "table",
        "--query",
        "pipeline",
        "--at-cycle",
        "13",
        "--signal",
        "tb_top.a",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 7);
    assert!(rows[0]["values"]["tb_top.a"].is_object());
    assert!(rows[0]["values"].get("tb_top.q1").is_none());
}

// ---------------------------------------------------------------------------
// T10: error paths
// ---------------------------------------------------------------------------

#[test]
fn t10_error_paths() {
    let out = run(&["-f", "/etc/hostname", "info"]);
    out.expect_code(2)
        .expect_stderr("unknown file format")
        .expect_stderr("log_vcd");

    let out = run(&["-f", VCD, "--no-config", "clock", "--clock", "nope"]);
    out.expect_code(2).expect_stderr("no signal \"nope\"");

    let out = vcd(&["value", "--at", "0", "--signal", "tb_top.qqq"]);
    out.expect_code(2)
        .expect_stderr("no signal \"tb_top.qqq\"")
        .expect_stderr("did you mean")
        .expect_stderr("tb_top.q1");

    let out = vcd(&["value", "--signal", "tb_top.a"]);
    out.expect_code(2).expect_stderr("--at");

    let out = vcd(&["value", "--at", "0", "--cycle", "3", "--signal", "tb_top.a"]);
    out.expect_code(2).expect_stderr("mutually exclusive");

    let out = vcd(&["value", "--cycle", "99", "--signal", "tb_top.a"]);
    out.expect_code(2).expect_stderr("out of range");

    let out = vcd(&["compare"]);
    out.expect_code(2).expect_stderr("no comparison pairs");

    let out = vcd(&["compare", "--map", "tb_top.ref_y=tb_top.q3"]);
    out.expect_code(2).expect_stderr("specify a shift");

    let out = vcd(&[
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--from",
        "0ns",
    ]);
    out.expect_code(2)
        .expect_stderr("cycle range starts before the first clock edge");

    let out = vcd(&[
        "compare",
        "--align",
        "transfers",
        "--map",
        "tb_top.ref_y=tb_top.q3",
    ]);
    out.expect_code(2).expect_stderr("requires --in-valid");

    let out = vcd(&[
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--align",
        "transfers",
        "--in-valid",
        "tb_top.in_valid",
        "--out-valid",
        "tb_top.out_go",
        "--gate",
        "tb_top.in_valid",
    ]);
    out.expect_code(2).expect_stderr("--gate is not meaningful");

    let out = vcd(&["signals", "--match", "["]);
    out.expect_code(2).expect_stderr("invalid pattern");

    let out = vcd(&[
        "value",
        "--at",
        "10ns",
        "--signal",
        "tb_top.a",
        "--signed-sig",
        "tb_top.nope",
    ]);
    out.expect_code(2)
        .expect_stderr("no signal \"tb_top.nope\"");

    let out = run(&["-f", VCD, "--no-config", "info", "--format", "yaml"]);
    out.expect_code(2);
}

// ---------------------------------------------------------------------------
// x/z handling
// ---------------------------------------------------------------------------

#[test]
fn x_values_and_x_modes() {
    // dec rendering of a vector with x/z bits
    let out = run(&[
        "-f",
        XVCD,
        "--no-config",
        "value",
        "--at",
        "5ns",
        "--signal",
        "tb.v",
        "--radix",
        "dec",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["rows"][0]["value"]["value"], "x");
    assert_eq!(doc["rows"][0]["value"]["bits"], "00xz");
    assert_eq!(doc["rows"][0]["value"]["has_x"], true);

    // default: x counts as a divergence
    let out = run(&[
        "-f",
        XVCD,
        "--no-config",
        "compare",
        "--map",
        "tb.v=tb.v:0",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["x_involved"], 1);
    assert_eq!(doc["pairs"][0]["mismatches"], 1);
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cmp"], "x-involved");
    assert_eq!(rows[0]["cycle"], 0);

    // --x wildcard: reported but not a divergence
    let out = run(&[
        "-f",
        XVCD,
        "--no-config",
        "compare",
        "--map",
        "tb.v=tb.v:0",
        "--x",
        "wildcard",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["x_involved"], 1);
    assert_eq!(doc["pairs"][0]["mismatches"], 0);
    assert_eq!(doc["pairs"][0]["status"], "ok");
    assert_eq!(table_rows(&doc).len(), 0);

    // --at-edges-only keeps only changes exactly at an edge
    let out = run(&[
        "-f",
        XVCD,
        "--no-config",
        "changes",
        "--signal",
        "tb.v",
        "--at-edges-only",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let cycles: Vec<i64> = table_rows(&doc)
        .iter()
        .map(|r| r["cycle"].as_i64().unwrap())
        .collect();
    assert_eq!(cycles, vec![0, 2]);

    // --no-initial drops the initial value of each signal
    let out = run(&[
        "-f",
        XVCD,
        "--no-config",
        "changes",
        "--signal",
        "tb.v",
        "--no-initial",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["tick"], 5);
}

// ---------------------------------------------------------------------------
// output formats, clock and introspection
// ---------------------------------------------------------------------------

#[test]
fn jsonl_records_for_align_and_compare() {
    let out = vcd(&[
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "jsonl",
    ]);
    out.expect_code(1);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(
        lines[0].starts_with(r#"{"record":"header","file":"tests/fixtures/data/pipeline.vcd""#),
        "{}",
        lines[0]
    );
    let mut saw_row = false;
    let mut saw_footer = false;
    let mut saw_pairs = false;
    for line in &lines {
        if line.starts_with(r#"{"record":"row","#) {
            saw_row = true;
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["cycle"], 13);
            assert_eq!(v["b"]["bits"], "000101101");
        }
        if line.starts_with(r#"{"record":"footer","#) {
            saw_footer = true;
        }
        if line.starts_with(r#"{"record":"pairs","#) {
            saw_pairs = true;
        }
    }
    assert!(
        saw_row && saw_footer && saw_pairs,
        "missing records:\n{}",
        out.stdout
    );
}

#[test]
fn clock_reports_edges_and_period() {
    let out = vcd(&["clock", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["signal"], "tb_top.clk");
    assert_eq!(doc["edge"], "posedge");
    assert_eq!(doc["edges"], 46);
    assert_eq!(doc["regular"], true);
    assert_eq!(doc["period_median"], 10000, "10ns at a 1ps timescale");
    assert_eq!(doc["candidates"][0]["full_name"], "tb_top.clk");
    assert_eq!(doc["detected_by"], "name");

    // negative edge halves the cycle count of the same dump
    let out = vcd(&["--edge", "negedge", "clock", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["edge"], "negedge");
    assert_eq!(doc["edges"], 46);
    assert_eq!(doc["signal"], "tb_top.clk");
}

#[test]
fn introspection_surfaces() {
    let out = vcd(&["info", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["format"], "vcd");
    assert_eq!(doc["timescale"]["factor"], 1);
    assert_eq!(doc["timescale"]["unit"], "ps");
    assert!(doc["vars"].as_u64().unwrap() >= 19);
    assert!(doc["body_bytes"].as_u64().unwrap() > 0);

    let out = vcd(&["info", "--full", "--with-clock", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["tick_count"], 93);
    assert_eq!(doc["first_tick"], 0);
    assert_eq!(doc["last_tick"], 460000);
    assert_eq!(doc["last_time"], "460ns");
    assert_eq!(doc["clock"]["edges"], 46);

    let out = vcd(&["scopes", "--depth", "0", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["full_name"], "tb_top");
    assert_eq!(rows[0]["type"], "module");
    assert_eq!(rows[0]["scopes"], 1, "tb_top.u_ref");

    let out = vcd(&[
        "signals",
        "--scope",
        "tb_top.u_ref",
        "--count",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    assert_eq!(out.json()["count"], 3);

    let out = vcd(&["signals", "--match", "tb_top.q*", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    let names: Vec<String> = table_rows(&doc)
        .iter()
        .map(|r| r["full_name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["tb_top.q1", "tb_top.q2", "tb_top.q3"]);
    assert_eq!(table_rows(&doc)[0]["width"], 9);
    assert_eq!(table_rows(&doc)[0]["encoding"], "bitvector(9)");

    // a scope name is not a signal
    let out = vcd(&["value", "--at", "0", "--signal", "tb_top"]);
    out.expect_code(2).expect_stderr("is a scope, not a signal");

    // signal details
    let out = vcd(&["signal", "tb_top.q3", "--format", "json"]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["full_name"], "tb_top.q3");
    assert_eq!(doc["width"], 9);
    assert_eq!(doc["changes"], 32);
    assert_eq!(doc["first_change"]["cycle"], serde_json::Value::Null);
    assert_eq!(doc["first_change"]["tick"], 0);
    assert_eq!(doc["last_change"]["cycle"], 45);
    assert_eq!(doc["signed_source"], "inferred");
    assert_eq!(doc["radix"], "auto");
}

#[test]
fn table_clock_none_and_changes_delta() {
    let out = vcd(&[
        "table",
        "--clock",
        "none",
        "--signal",
        "tb_top.ref_y",
        "--time",
        "125ns..145ns",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert!(doc["clock"].is_null());
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 5);
    assert!(
        rows[0].get("cycle").is_none(),
        "tick-domain rows have no cycle"
    );
    assert_eq!(rows[0]["tick"], 125000);

    let out = vcd(&[
        "changes",
        "--signal",
        "tb_top.q3",
        "--delta",
        "--from-cycle",
        "0",
        "--to-cycle",
        "8",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows[0]["cycle"], 3);
    assert!(
        rows[0]["delta_cycles"].is_null(),
        "first row has no previous cycle"
    );
    assert_eq!(rows[1]["delta_cycles"], 1);
    assert_eq!(rows[1]["cycle"], 4);
}

// ---------------------------------------------------------------------------
// Wave 1: cross-file comparison (--b-file), cycle offsets and anchors
// ---------------------------------------------------------------------------

const TWO: &str = "tests/fixtures/data/two_scopes.vcd";

#[test]
fn cross_file_compares_two_dumps() {
    // side A from the VCD, side B from the FST of the same simulation
    let out = run(&[
        "-f",
        VCD,
        "--b-file",
        FST,
        "--no-config",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["file"], VCD);
    assert_eq!(doc["b_file"], FST, "the header must name both dumps");
    assert_eq!(doc["pairs"][0]["mismatches"], 1);
    assert_eq!(doc["pairs"][0]["first_divergent_cycle"], 13);
    assert_eq!(table_rows(&doc)[0]["b"]["bits"], "000101101");

    // a self-pair across two formats is bit-exact
    let out = run(&[
        "-f",
        VCD,
        "--b-file",
        FST,
        "--no-config",
        "compare",
        "--map",
        "tb_top.q3=tb_top.q3",
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["compared"], 46);
    assert_eq!(doc["pairs"][0]["mismatches"], 0);

    // align also works across files
    let out = run(&[
        "-f",
        VCD,
        "--b-file",
        FST,
        "--no-config",
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--shifts",
        "0..=4",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    assert_eq!(out.json()["best_shift"], 3);

    // a GHW dump works as side B too (all three formats are first-class sides)
    let out = run(&[
        "-f",
        GHW,
        "--b-file",
        GHW,
        "--no-config",
        "compare",
        "--map",
        "counter_tb.cnt=counter_tb.cnt",
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["compared"], 20);
    assert_eq!(doc["pairs"][0]["mismatches"], 0);
    assert_eq!(doc["b_file"], GHW);
    assert_eq!(doc["b_clock"], "counter_tb.clk");

    // B-side flags on a single-dump command are an error, not a silent no-op
    let out = run(&[
        "-f",
        VCD,
        "--b-file",
        FST,
        "--no-config",
        "table",
        "--signal",
        "tb_top.a",
    ]);
    out.expect_code(2).expect_stderr("need two sides");
    let out = run(&[
        "-f",
        VCD,
        "--b-cycle-offset",
        "3",
        "--no-config",
        "changes",
        "--signal",
        "tb_top.a",
    ]);
    out.expect_code(2).expect_stderr("need two sides");
}

#[test]
fn b_cycle_offset_absorbs_a_known_skew() {
    // B's local edge 3 becomes logical cycle 0, so shift 0 is the aligned view
    let out = run(&[
        "-f",
        VCD,
        "--b-file",
        VCD,
        "--b-cycle-offset",
        "3",
        "--no-config",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3",
        "--shift",
        "0",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["b_cycle_origin"], 3);
    assert_eq!(doc["pairs"][0]["first_divergent_cycle"], 10);
    let row = &table_rows(&doc)[0];
    assert_eq!(
        row["a_cycle"], row["b_cycle"],
        "the offset absorbs the latency"
    );
    assert_eq!(row["b_cycle_local"], 13, "local B cycle = logical + origin");
    assert_eq!(row["time"], "135ns", "times stay absolute");
}

#[test]
fn anchor_redefines_cycle_zero_everywhere() {
    // --anchor moves cycle 0 to the first cycle where the signal is 1
    let out = vcd(&[
        "--anchor",
        "tb_top.stall",
        "table",
        "--cycles",
        "0..2",
        "--signal",
        "tb_top.a",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows[0]["cycle"], 0);
    assert_eq!(
        rows[0]["tick"], 205000,
        "cycle 0 is the stall edge (local 20)"
    );
    assert_eq!(
        rows[0]["values"]["tb_top.a"]["value"], "14",
        "hex of a_20 = 20"
    );

    // compare reports the origin and works inside the new frame
    let out = vcd(&[
        "--anchor",
        "tb_top.stall",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["a_cycle_origin"], 20);
    assert_eq!(doc["to_cycle"], 25, "45 - 20");
    let first = doc["pairs"][0]["first_divergent_cycle"].as_i64().unwrap();
    assert!(
        (0..5).contains(&first),
        "the pre-stall bug is now before cycle 0: {first}"
    );
    assert_eq!(doc["pairs"][0]["compared"], 23);

    // changes work in the anchored frame too (nothing changes while stalled)
    let out = vcd(&[
        "--anchor",
        "tb_top.stall",
        "changes",
        "--signal",
        "tb_top.a",
        "--from-cycle",
        "0",
        "--to-cycle",
        "2",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    assert_eq!(table_rows(&out.json()).len(), 0);

    // a signal that is already 1 at the first edge keeps the default origin
    let out = vcd(&[
        "--anchor",
        "tb_top.in_valid",
        "table",
        "--cycles",
        "0..0",
        "--signal",
        "tb_top.a",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert!(doc.get("a_cycle_origin").is_none() || doc["a_cycle_origin"] == 0);
    assert_eq!(table_rows(&doc)[0]["values"]["tb_top.a"]["value"], "01");

    // `rst` is only high before the first edge, so it can never be an anchor
    let out = vcd(&["--anchor", "tb_top.rst", "table", "--signal", "tb_top.a"]);
    out.expect_code(2).expect_stderr("never asserted");

    // both sides anchored explicitly on one shared dump stay in step
    let out = run(&[
        "-f",
        VCD,
        "--anchor",
        "tb_top.stall",
        "--b-anchor",
        "tb_top.stall",
        "--no-config",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["a_cycle_origin"], 20);
    assert_eq!(doc["b_cycle_origin"], 20);
    assert_eq!(doc["pairs"][0]["compared"], 23);

    // errors: unknown signal, and a signal that is never 1
    let out = vcd(&["--anchor", "tb_top.qqq", "table", "--signal", "tb_top.a"]);
    out.expect_code(2).expect_stderr("no signal \"tb_top.qqq\"");
    let out = vcd(&["--anchor", "tb_top.sn", "table", "--signal", "tb_top.a"]);
    out.expect_code(2).expect_stderr("never asserted");
}

// ---------------------------------------------------------------------------
// Wave 2: pairing a whole scope without writing every --map
// ---------------------------------------------------------------------------

#[test]
fn pair_scopes_and_map_glob_pair_by_name() {
    let expected_pairs = vec![
        "tb.dut.a0",
        "tb.dut.b0",
        "tb.dut.lane0.x",
        "tb.dut.lane1.x",
        "tb.dut.sum",
    ];

    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--pair-scopes",
        "tb.dut,tb.ref",
        "--pair-by",
        "path",
        "--shift",
        "0",
        "--delta",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pair_count"], 5);
    assert_eq!(doc["mismatches"], 2);
    let names: Vec<&str> = doc["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["a"].as_str().unwrap())
        .collect();
    assert_eq!(names, expected_pairs, "sorted by pairing key");

    // a signal that exists on one side only is reported, not silently ignored
    assert_eq!(doc["unpaired_a_signals"], 1);
    assert_eq!(doc["unpaired_a"][0], "tb.dut.only");
    assert_eq!(doc["unpaired_b_signals"], 1);
    assert_eq!(doc["unpaired_b"][0], "tb.ref.extra");
    assert!(out.stderr.contains("nothing to compare"), "{}", out.stderr);

    // the same pairing via --map-glob
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map-glob",
        "tb.dut.*=tb.ref.*",
        "--pair-by",
        "path",
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pair_count"], 5);
    assert_eq!(doc["mismatches"], 2);
    let cycles: Vec<i64> = table_rows(&doc)
        .iter()
        .map(|r| r["cycle"].as_i64().unwrap())
        .collect();
    assert_eq!(cycles, vec![5, 6]);

    // leaf pairing on this fixture is ambiguous and says how to fix it
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--pair-scopes",
        "tb.dut,tb.ref",
        "--shift",
        "0",
    ]);
    out.expect_code(2)
        .expect_stderr("ambiguous")
        .expect_stderr("--pair-by path");

    // a glob in --map is a mistake worth naming
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        "tb.dut.*=tb.ref.*",
        "--shift",
        "0",
    ]);
    out.expect_code(2).expect_stderr("looks like a glob");
}

#[test]
fn config_compare_pairs_section() {
    let path = std::env::temp_dir().join(format!("waveq_pairs_{}.toml", std::process::id()));
    let two_abs = std::fs::canonicalize(TWO).unwrap();
    std::fs::write(
        &path,
        format!(
            "[wave]\nfile = \"{}\"\n\n[compare]\nshift = 0\n\n[compare.pairs]\n\"tb.dut.*\" = \"tb.ref.*\"\n",
            two_abs.display()
        ),
    )
    .unwrap();
    // `shift` is not a [compare] key: it must be rejected loudly (no silent typos)
    let out = run(&["--config", path.to_str().unwrap(), "info"]);
    out.expect_code(2).expect_stderr("unknown config key");

    std::fs::write(
        &path,
        format!(
            "[wave]\nfile = \"{}\"\n\n[compare]\nmax_delta = 1\n\n[compare.pairs]\n\"tb.dut.*\" = \"tb.ref.*\"\n",
            two_abs.display()
        ),
    )
    .unwrap();
    let out = run(&[
        "--config",
        path.to_str().unwrap(),
        "compare",
        "--pair-by",
        "path",
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pair_count"], 5);
    assert_eq!(doc["max_delta"], 1, "[compare] max_delta applies");
    // the +1 sum flip is inside the bound, the -9 lane1 divergence is not
    assert_eq!(doc["mismatches"], 1);
    let divergent: Vec<&str> = doc["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["status"] == "divergent")
        .map(|p| p["a"].as_str().unwrap())
        .collect();
    assert_eq!(divergent, vec!["tb.dut.lane1.x"]);
    std::fs::remove_file(&path).ok();
}

// ---------------------------------------------------------------------------
// Wave 3: numeric delta columns, masks and a documented bound
// ---------------------------------------------------------------------------

#[test]
fn delta_columns_and_summary() {
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--pair-scopes",
        "tb.dut,tb.ref",
        "--pair-by",
        "path",
        "--shift",
        "0",
        "--delta",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    let rows = table_rows(&doc);
    assert_eq!(rows.len(), 2);
    let sum_row = rows.iter().find(|r| r["pair"] == 4).unwrap();
    assert_eq!(sum_row["cycle"], 5);
    assert_eq!(sum_row["delta"], -1);
    assert_eq!(sum_row["bits_differ"], "01f");
    assert_eq!(sum_row["diff_bits"], 5);
    let lane_row = rows.iter().find(|r| r["pair"] == 3).unwrap();
    assert_eq!(lane_row["delta"], -9);
    assert_eq!(lane_row["bits_differ"], "9");
    assert_eq!(lane_row["diff_bits"], 2);

    let pairs = doc["pairs"].as_array().unwrap();
    let sum_pair = pairs.iter().find(|p| p["a"] == "tb.dut.sum").unwrap();
    assert_eq!(sum_pair["max_abs_delta"], 1);
    assert_eq!(sum_pair["max_bits_differ"], 5);
    assert_eq!(sum_pair["cmp"], "bit-exact");
    // the mask is not in the text table unless --delta is given
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--pair-scopes",
        "tb.dut,tb.ref",
        "--pair-by",
        "path",
        "--shift",
        "0",
    ]);
    let main_header = out
        .stdout
        .lines()
        .find(|l| l.starts_with("cycle") || l.starts_with("transfer"))
        .expect("main table header");
    assert!(
        !main_header.contains("bits_differ") && !main_header.contains("delta"),
        "delta columns are opt-in: {main_header}"
    );

    // signed sides change the delta sign (dut.lane1.x = 9 -> -7 as a 4-bit signed value)
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "--signed",
        "compare",
        "--pair-scopes",
        "tb.dut,tb.ref",
        "--pair-by",
        "path",
        "--shift",
        "0",
        "--delta",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    let lane = table_rows(&doc)
        .into_iter()
        .find(|r| r["pair"] == 3)
        .unwrap();
    assert_eq!(lane["delta"], 7, "0 - (-7) with both sides signed");
}

#[test]
fn max_delta_turns_a_metric_into_the_rule() {
    // bit-exact by default: the injected +1 is a divergence
    let out = run(&[
        "-f",
        VCD,
        "--no-config",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    assert_eq!(out.json()["pairs"][0]["mismatches"], 1);

    // tolerance 1: no divergence, exit 0, rule reported
    let out = run(&[
        "-f",
        VCD,
        "--no-config",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--max-delta",
        "1",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["max_delta"], 1);
    assert_eq!(doc["divergence_rule"], "|delta| > 1");
    assert_eq!(doc["pairs"][0]["mismatches"], 0);
    assert_eq!(doc["pairs"][0]["max_abs_delta"], 1);

    // tolerance 0 is exact equality again
    let out = run(&[
        "-f",
        VCD,
        "--no-config",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--max-delta",
        "0",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    assert_eq!(out.json()["pairs"][0]["mismatches"], 1);

    // align uses the same rule, so a shift scan can be done against a bound
    let out = run(&[
        "-f",
        VCD,
        "--no-config",
        "align",
        "--a",
        "tb_top.ref_y",
        "--b",
        "tb_top.q3",
        "--shifts",
        "0..=8",
        "--max-delta",
        "1",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["best_shift"], 3);
    assert_eq!(doc["best_mismatches"], 0);
    assert_eq!(doc["divergence_rule"], "|delta| > 1");
    assert_eq!(doc["max_abs_delta"], 1);
}

#[test]
fn probe_at_chooses_the_sampled_cycle() {
    let out = vcd(&[
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--probe",
        "tb_top.a",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    let row = &table_rows(&doc)[0];
    assert_eq!(row["probe_at"], "a_cycle");
    assert_eq!(row["probe"]["tb_top.a"]["value"], "0b", "a_10 = 11");

    let out = vcd(&[
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--probe",
        "tb_top.a",
        "--probe-at",
        "b-cycle",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    let row = &table_rows(&doc)[0];
    assert_eq!(row["probe_at"], "b_cycle");
    assert_eq!(
        row["probe"]["tb_top.a"]["value"], "0e",
        "a_13 = 14 at the divergent DUT cycle"
    );
    assert_eq!(row["a_cycle"], 10);
    assert_eq!(row["b_cycle"], 13);
}

// ---------------------------------------------------------------------------
// Wave 4: expected values from a file (no signal on side B at all)
// ---------------------------------------------------------------------------

fn write_expect_csv(name: &str, body: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("waveq_{}_{}.csv", name, std::process::id()));
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn expect_file_is_a_b_side() {
    // dut.sum = 3c, but the injected bug makes cycle 5 equal 16
    let csv = write_expect_csv(
        "golden",
        "cycle,dout\n0,0\n1,3\n2,6\n3,9\n4,12\n5,15\n6,18\n7,21\n",
    );
    let map = format!("tb.dut.sum=@{}:cycle,dout", csv.display());
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        &map,
        "--shift",
        "0",
        "--delta",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pair_count"], 1);
    assert_eq!(doc["mismatches"], 1);
    let row = &table_rows(&doc)[0];
    assert_eq!(row["cycle"], 5);
    assert_eq!(row["a"]["value"], "010");
    assert_eq!(row["b"]["value"], "00f");
    assert_eq!(
        row["delta"], -1,
        "the DUT is one higher than the golden value"
    );
    assert_eq!(row["pair"], 0);

    // the same through --expect-file with a `@:cols` B side
    let out = run(&[
        "-f",
        TWO,
        "--expect-file",
        csv.to_str().unwrap(),
        "--no-config",
        "compare",
        "--map",
        "tb.dut.sum=@:cycle,dout",
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    assert_eq!(out.json()["mismatches"], 1);

    // the golden file is a *bound* away from failing: --max-delta 1 accepts it
    let out = run(&[
        "-f",
        TWO,
        "--expect-file",
        csv.to_str().unwrap(),
        "--no-config",
        "compare",
        "--map",
        "tb.dut.sum=@:cycle,dout",
        "--shift",
        "0",
        "--max-delta",
        "1",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["mismatches"], 0);
    assert_eq!(doc["divergence_rule"], "|delta| > 1");

    std::fs::remove_file(&csv).ok();
}

#[test]
fn expect_file_edge_cases() {
    // partial coverage: uncovered cycles are skipped and reported
    let short = write_expect_csv("short", "cycle,dout\n0,0\n1,3\n");
    let map = format!("tb.dut.sum=@{}:cycle,dout", short.display());
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        &map,
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["compared"], 2);
    assert_eq!(doc["pairs"][0]["skipped"], 6);
    assert!(out.stderr.contains("not covered"), "{}", out.stderr);
    std::fs::remove_file(&short).ok();

    // a value that does not fit the compared width is an error, not a silent skip
    let bad = write_expect_csv("bad", "cycle,dout\n0,999999\n");
    let map = format!("tb.dut.sum=@{}:cycle,dout", bad.display());
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        &map,
        "--shift",
        "0",
    ]);
    out.expect_code(2).expect_stderr("does not fit in 9 bits");
    std::fs::remove_file(&bad).ok();

    // duplicate cycles are refused
    let dup = write_expect_csv("dup", "cycle,dout\n1,0\n1,1\n");
    let map = format!("tb.dut.sum=@{}:cycle,dout", dup.display());
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        &map,
        "--shift",
        "0",
    ]);
    out.expect_code(2).expect_stderr("already has a value");
    std::fs::remove_file(&dup).ok();

    // x/z values are honoured (here they make the unit x-involved)
    let xz = write_expect_csv("xz", "cycle,dout\n0,00xz\n1,3\n");
    let map = format!("tb.dut.sum=@{}:cycle,dout", xz.display());
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        &map,
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["pairs"][0]["x_involved"], 1);
    assert_eq!(table_rows(&doc)[0]["cmp"], "x-involved");
    std::fs::remove_file(&xz).ok();

    // JSONL expected values work the same way
    let jsonl = std::env::temp_dir().join(format!("waveq_golden_{}.jsonl", std::process::id()));
    std::fs::write(
        &jsonl,
        "{\"cycle\":0,\"dout\":0}\n{\"cycle\":1,\"dout\":3}\n{\"cycle\":2,\"dout\":6}\n\
         {\"cycle\":3,\"dout\":9}\n{\"cycle\":4,\"dout\":12}\n{\"cycle\":5,\"dout\":15}\n\
         {\"cycle\":6,\"dout\":18}\n{\"cycle\":7,\"dout\":21}\n",
    )
    .unwrap();
    let map = format!("tb.dut.sum=@{}:cycle,dout", jsonl.display());
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        &map,
        "--shift",
        "0",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    assert_eq!(out.json()["mismatches"], 1);
    std::fs::remove_file(&jsonl).ok();

    // `@` without --expect-file says so
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        "tb.dut.sum=@:cycle,dout",
        "--shift",
        "0",
    ]);
    out.expect_code(2).expect_stderr("--expect-file");

    // a missing file is an error
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "compare",
        "--map",
        "tb.dut.sum=@/nonexistent/g.csv:cycle,dout",
        "--shift",
        "0",
    ]);
    out.expect_code(2).expect_stderr("cannot read");
}

#[test]
fn signals_tree_view() {
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "signals",
        "--tree",
        "--match",
        "tb.dut.*",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let doc = out.json();
    let rows = table_rows(&doc);
    let scopes: Vec<(&str, i64)> = rows
        .iter()
        .filter(|r| r["kind"] == "scope")
        .map(|r| {
            (
                r["full_name"].as_str().unwrap(),
                r["depth"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        scopes,
        vec![
            ("tb", 1),
            ("tb.dut", 2),
            ("tb.dut.lane0", 3),
            ("tb.dut.lane1", 3)
        ]
    );

    // signals stay under their scope and pick up indentation in the text view
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "signals",
        "--tree",
        "--scope",
        "tb.dut",
    ]);
    out.expect_code(0);
    assert!(out.stdout.contains("\ntb.dut  scope"), "{}", out.stdout);
    assert!(out.stdout.contains("\n  a0"), "{}", out.stdout);
    assert!(out.stdout.contains("\n    x"), "{}", out.stdout);
    assert!(!out.stdout.contains("tb.ref"), "{}", out.stdout);

    // an unknown scope root is an error
    let out = run(&[
        "-f",
        TWO,
        "--no-config",
        "signals",
        "--tree",
        "--scope",
        "tb.nope",
    ]);
    out.expect_code(2).expect_stderr("no scope");
}

#[test]
fn cut_rejects_the_rebasing_flags() {
    // the window is expressed in the file's own cycles; re-basing flags would be
    // silently meaningless here
    let out = vcd(&[
        "cut",
        "--cycles",
        "11..12",
        "--cycle-offset",
        "5",
        "--signal",
        "tb_top.q3",
    ]);
    out.expect_code(2).expect_stderr("not supported here");
    let out = vcd(&[
        "cut",
        "--cycles",
        "11..12",
        "--anchor",
        "tb_top.in_valid",
        "--signal",
        "tb_top.q3",
    ]);
    out.expect_code(2).expect_stderr("not supported here");

    // ... and a cut is read back with the source's numbering via --cycle-offset,
    // in both the table and the comparison commands
    let path = std::env::temp_dir().join(format!("waveq_window_{}.vcd", std::process::id()));
    let out = vcd(&[
        "cut",
        "--cycles",
        "10..13",
        "--signal",
        "tb_top.ref_y,tb_top.q3",
        "-o",
        path.to_str().unwrap(),
    ]);
    out.expect_code(0);
    assert!(out.stdout.contains("read_with"), "{}", out.stdout);
    assert!(out.stderr.contains("--cycle-offset -10"), "{}", out.stderr);

    // a four-cycle window has fewer edges than clock auto-detection needs, so the
    // clock is named explicitly (a real window is long enough not to care)
    let out = run(&[
        "-f",
        path.to_str().unwrap(),
        "--no-config",
        "--clock",
        "tb_top.clk",
        "--cycle-offset",
        "-10",
        "table",
        "--cycles",
        "10..13",
        "--signal",
        "tb_top.ref_y,tb_top.q3",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let rows = table_rows(&out.json());
    assert_eq!(rows[0]["cycle"], 10);
    assert_eq!(rows[3]["cycle"], 13);
    assert_eq!(
        rows[0]["values"]["tb_top.ref_y"]["bits"], "000101100",
        "source cycle 10: ref_y = 44"
    );
    assert_eq!(
        rows[3]["values"]["tb_top.q3"]["bits"], "000101101",
        "source cycle 13: the corrupted q3 = 45"
    );

    let out = run(&[
        "-f",
        path.to_str().unwrap(),
        "--no-config",
        "--clock",
        "tb_top.clk",
        "--cycle-offset",
        "-10",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--from-cycle",
        "10",
        "--to-cycle",
        "13",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(doc["a_cycle_origin"], -10);
    assert_eq!(
        doc["pairs"][0]["compared"], 1,
        "only cycle 10 has both sides in range"
    );
    assert_eq!(doc["pairs"][0]["first_divergent_cycle"], 13);
    let row = &table_rows(&doc)[0];
    assert_eq!(row["a_cycle"], 10);
    assert_eq!(row["b_cycle"], 13);
    assert_eq!(row["b"]["bits"], "000101101");
    std::fs::remove_file(&path).ok();
}

#[test]
fn cycle_offset_shifts_numbering_like_an_anchor() {
    // --cycle-offset 20 skips the first 20 local cycles, exactly like anchoring
    // on the first cycle where tb_top.stall is 1
    let by_offset = vcd(&[
        "--cycle-offset",
        "20",
        "table",
        "--cycles",
        "0..2",
        "--signal",
        "tb_top.a",
        "--format",
        "json",
    ]);
    by_offset.expect_code(0);
    let by_anchor = vcd(&[
        "--anchor",
        "tb_top.stall",
        "table",
        "--cycles",
        "0..2",
        "--signal",
        "tb_top.a",
        "--format",
        "json",
    ]);
    by_anchor.expect_code(0);
    assert_eq!(table_rows(&by_offset.json()), table_rows(&by_anchor.json()));
    assert_eq!(table_rows(&by_offset.json())[0]["tick"], 205000);

    // a negative offset moves numbering back: cycle 5 is the file's local cycle 0
    let out = vcd(&[
        "--cycle-offset",
        "-5",
        "table",
        "--cycles",
        "5..6",
        "--signal",
        "tb_top.a",
        "--format",
        "json",
    ]);
    out.expect_code(0);
    let rows = table_rows(&out.json());
    assert_eq!(rows[0]["cycle"], 5);
    assert_eq!(rows[0]["tick"], 5000, "local cycle 0 is the first posedge");
    assert_eq!(rows[0]["values"]["tb_top.a"]["value"], "01");

    // cycles that do not exist in the file are an error, not a crash
    let out = vcd(&[
        "--cycle-offset",
        "-5",
        "table",
        "--cycles",
        "0..0",
        "--signal",
        "tb_top.a",
    ]);
    out.expect_code(2).expect_stderr("first cycle here is 5");

    // compare honours it too: the origin is reported
    let out = vcd(&[
        "--cycle-offset",
        "20",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3:3",
        "--format",
        "json",
    ]);
    out.expect_code(1);
    assert_eq!(out.json()["a_cycle_origin"], 20);
}
