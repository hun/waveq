//! End-to-end tests for script mode (`waveq run` / `waveq eval`).
//!
//! They shell out to the real binary, like `tests/cli.rs`, and use the same
//! fixture numbers (see `tests/fixtures/src/pipeline.sv`):
//!
//!   * cycle c is the posedge at tick 5+10c ns (timescale 1ps, so tick 135000
//!     is 135ns); sampling is post-edge
//!   * ref_y_c = 4(c+1), q3_c = ref_y[c-3] except for the injected bug, so
//!     ref_y[13] = 0x038 = 56 and q3[13] = 0x02d = 45
//!   * the 12-cycle stall (cycles 20..31) freezes the pipeline, so the tail of
//!     a cycle-space window diverges by design

use serde_json::Value;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_waveq");
const VCD: &str = "tests/fixtures/data/pipeline.vcd";
const GHW: &str = "tests/fixtures/data/counter.ghw";
const XZ: &str = "tests/fixtures/data/x_values.vcd";
const TWO: &str = "tests/fixtures/data/two_scopes.vcd";
const CFG: &str = "tests/fixtures/waveq.toml";
const GOLDEN: &str = "tests/fixtures/data/golden_ref_y.json";
const SCRIPT: &str = "tests/fixtures/scripts/cycles.lua";

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

    fn expect_stdout(&self, needle: &str) -> &Out {
        assert!(
            self.stdout.contains(needle),
            "stdout should contain {needle:?}, got:\n{}",
            self.stdout
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

/// `waveq -f <vcd> --no-config eval <code> <extra...>`.
///
/// `--budget`/`--memory` belong to the subcommand, so the extra arguments go
/// after the code.
fn eval_with(extra: &[&str], code: &str) -> Out {
    let mut args = vec!["-f", VCD, "--no-config", "eval", code];
    args.extend_from_slice(extra);
    run(&args)
}

/// `waveq -f <vcd> --no-config eval <code> --format json`.
fn eval(code: &str) -> Out {
    eval_with(&["--format", "json"], code)
}

fn rows(doc: &Value) -> Vec<Value> {
    doc["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("document has no rows array: {doc}"))
        .clone()
}

// ---------------------------------------------------------------------------
// S1: the API against the documented fixture numbers
// ---------------------------------------------------------------------------

#[test]
fn s1_api_basics() {
    let out = eval(
        r#"
assert(wave.cycles() == 46 and wave.last_cycle() == 45)
assert(wave.tick(13) == 135000 and wave.time(135000) == "135ns")
assert(wave.tick(999) == nil and wave.time(0) == "0s")
assert(wave.clock().edges == 46 and wave.clock().signal == "tb_top.clk")
assert(wave.clock().period_median == 10000 and wave.clock().edge == "posedge")
assert(#wave.signals("tb_top.q*") == 3 and #wave.signals() == 20)
assert(wave.info().format == "vcd" and wave.info().vars == 20)
assert(wave.info().timescale == "1ps" and wave.info().tick_count == 93)
local y = wave.signal("tb_top.ref_y")
assert(y.width == 9 and y:value(13) == "038" and y:int(13) == 56 and not y:is_xz(13))
assert(y.name == "tb_top.ref_y" and y.spec == "tb_top.ref_y" and y.index == "8:0")
assert(y.radix == "auto" and y.kind == "bitvec" and not y.signed)
assert(y:value(13, "dec") == "56" and y:value(13, "bin") == "000111000")
assert(y:bits(13) == "000111000" and y:at(999) == nil)
local q = wave.signals("tb_top.q3")[1]
assert(q.full_name == "tb_top.q3" and q.width == 9 and q.index == "8:0" and not q.signed)
wave.emit{cycle = 13, value = y:value(13), int = y:int(13)}
return true
"#,
    );
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["sides"], "a");
    assert_eq!(doc["script"], "=eval");
    assert_eq!(doc["file"], VCD);
    let rows = rows(&doc);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cycle"], 13);
    assert_eq!(rows[0]["value"], "038");
    assert_eq!(rows[0]["int"], 56);
}

#[test]
fn s2_changes_match_the_declarative_command() {
    let cli = run(&[
        "-f",
        VCD,
        "--no-config",
        "changes",
        "--signal",
        "tb_top.q3",
        "--from-cycle",
        "0",
        "--to-cycle",
        "4",
        "--format",
        "json",
    ]);
    cli.expect_code(0);
    let want: Vec<(i64, u64, String, String, bool)> = rows(&cli.json())
        .iter()
        .map(|r| {
            (
                r["cycle"].as_i64().unwrap(),
                r["tick"].as_u64().unwrap(),
                r["value"].as_str().unwrap().to_string(),
                r["bits"].as_str().unwrap().to_string(),
                r["has_x"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(want.len(), 2);

    let out = eval(
        r#"
local rows = wave.signal("tb_top.q3"):changes(0, 4)
assert(#rows == 2)
wave.emit{cycle = rows[1].cycle, tick = rows[1].tick, value = rows[1].value, bits = rows[1].bits, has_x = rows[1].has_x}
wave.emit{cycle = rows[2].cycle, tick = rows[2].tick, value = rows[2].value, bits = rows[2].bits, has_x = rows[2].has_x}
-- the initial tick precedes the first clock edge: cycle stays negative
assert(wave.signal("tb_top.q3"):changes()[1].cycle == -1)
return true
"#,
    );
    out.expect_code(0);
    let got: Vec<(i64, u64, String, String, bool)> = rows(&out.json())
        .iter()
        .map(|r| {
            (
                r["cycle"].as_i64().unwrap(),
                r["tick"].as_u64().unwrap(),
                r["value"].as_str().unwrap().to_string(),
                r["bits"].as_str().unwrap().to_string(),
                r["has_x"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, want);
}

// ---------------------------------------------------------------------------
// S2: the question the CLI cannot ask in one invocation
// ---------------------------------------------------------------------------

#[test]
fn s3_align_reproduced_in_a_script() {
    let cli = run(&[
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
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
        "--format",
        "json",
    ]);
    cli.expect_code(1);
    let cli = cli.json();
    let (cli_shift, cli_mismatches) = (
        cli["best_shift"].as_i64().unwrap(),
        cli["best_mismatches"].as_i64().unwrap(),
    );

    let out = eval(
        r#"
local y = wave.signal("tb_top.ref_y")
local q = wave.signal("tb_top.q3")
local best, best_m, best_pairs
for s = 0, 8 do
  local m, n = 0, 0
  for c = s, 19 do
    n = n + 1
    if y:bits(c - s) ~= q:bits(c) then m = m + 1 end
  end
  if best_m == nil or m < best_m then best, best_m, best_pairs = s, m, n end
end
wave.emit{shift = best, mismatches = best_m, pairs = best_pairs}
return true
"#,
    );
    out.expect_code(0);
    let rows = rows(&out.json());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["shift"], cli_shift);
    assert_eq!(rows[0]["mismatches"], cli_mismatches);
    // the documented outcome of the fixture's injected bug
    assert_eq!(rows[0]["shift"], 3);
    assert_eq!(rows[0]["mismatches"], 1);
    assert_eq!(rows[0]["pairs"], 17);
}

// ---------------------------------------------------------------------------
// S3: two sides
// ---------------------------------------------------------------------------

#[test]
fn s4_two_sides_with_a_b_cycle_offset() {
    // the CLI's own numbers for the same pair: a_cycle 10 (ref_y 02c) against
    // b_cycle 13 (q3 02d), reached with --auto-shift
    let cli = run(&[
        "-f",
        VCD,
        "--no-config",
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
    cli.expect_code(1);
    let cli = cli.json();
    let pair = &rows(&cli)[0];
    assert_eq!(pair["a_cycle"], 10);
    assert_eq!(pair["b_cycle"], 13);

    let out = eval_with(
        &["--format", "json", "--b-cycle-offset", "3"],
        r#"
local s = wave.sides()
assert(#s == 2 and s[1] == "a" and s[2] == "b")
local a = wave.signal("tb_top.ref_y")
local b = wave.signal("tb_top.q3", "b")
-- side B's logical cycle 10 is side A's cycle 13 (origin 3)
assert(wave.signal("tb_top.q3", "b"):at(10).bits == wave.signal("tb_top.q3"):at(13).bits)
assert(wave.path("b") == wave.path("a"))
local m, first = 0, nil
for c = 0, 16 do
  if a:bits(c) ~= b:bits(c) then m = m + 1; if first == nil then first = c end end
end
wave.emit{cycle = first, mismatches = m, a = a:bits(first), b = b:bits(first)}
return true
"#,
    );
    out.expect_code(0);
    let doc = out.json();
    assert_eq!(doc["sides"], "ab");
    let got = rows(&doc);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["cycle"], 10);
    assert_eq!(got[0]["mismatches"], 1);
    // the same divergence the CLI reports, in bit form
    assert_eq!(got[0]["a"], pair["a"]["bits"]);
    assert_eq!(got[0]["b"], pair["b"]["bits"]);

    // two anchors (both sides anchored on the same event) put B on the same
    // origin as A, so the pipeline latency has to come from the explicit
    // shift. The CLI reports both origins, and the script must agree on the
    // whole window: a[c-3] against b[c] for c = 3..19, first divergence at
    // cycle 10 (a_cycle 7 / b_cycle 10, b_cycle_local 13)
    let cli = run(&[
        "-f",
        VCD,
        "--no-config",
        "--anchor",
        "tb_top.out_go",
        "--b-anchor",
        "tb_top.out_go",
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
    cli.expect_code(1);
    let cli = cli.json();
    assert_eq!(cli["a_cycle_origin"], 3);
    assert_eq!(cli["b_cycle_origin"], 3);
    let pair = &rows(&cli)[0];
    assert_eq!(pair["cycle"], 10);
    assert_eq!(pair["a_cycle"], 7);
    assert_eq!(pair["b_cycle"], 10);
    assert_eq!(pair["b_cycle_local"], 13, "origin 3 on top of b_cycle 10");

    let out = eval_with(
        &[
            "--format",
            "json",
            "--anchor",
            "tb_top.out_go",
            "--b-anchor",
            "tb_top.out_go",
        ],
        r#"
local a = wave.signal("tb_top.ref_y")
local b = wave.signal("tb_top.q3", "b")
local m, first = 0, nil
for c = 3, 19 do
  if a:bits(c - 3) ~= b:bits(c) then
    m = m + 1
    if first == nil then first = c end
  end
end
wave.emit{cycle = first, mismatches = m, a = a:bits(first - 3), b = b:bits(first)}
return true
"#,
    );
    out.expect_code(0);
    let got = rows(&out.json());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["cycle"], pair["b_cycle"]);
    assert_eq!(got[0]["mismatches"], cli["mismatches"]);
    assert_eq!(got[0]["a"], pair["a"]["bits"]);
    assert_eq!(got[0]["b"], pair["b"]["bits"]);
}

// ---------------------------------------------------------------------------
// S4: exit codes, errors and the sandbox
// ---------------------------------------------------------------------------

#[test]
fn s5_exit_codes_and_errors() {
    // false -> 1, and nothing on stderr
    let out = eval("return false");
    out.expect_code(1);
    assert_eq!(out.stderr, "");
    assert_eq!(out.json()["rows"], Value::Null, "no emit -> no table");

    // any other return value is a usage error naming the type
    eval("return 5").expect_code(2).expect_stderr("integer");
    eval("return 'x'").expect_code(2).expect_stderr("string");

    // runtime errors carry the message and the chunk name
    eval("error('boom')")
        .expect_code(2)
        .expect_stderr("boom")
        .expect_stderr("eval:1");
    eval("local x = 1 +")
        .expect_code(2)
        .expect_stderr("syntax error");
    eval("this is not lua")
        .expect_code(2)
        .expect_stderr("eval:1");

    // a file's errors name the file
    let out = run(&[
        "-f",
        VCD,
        "--no-config",
        "run",
        "tests/fixtures/scripts/nope.lua",
    ]);
    out.expect_code(2).expect_stderr("cannot read script");

    // unknown [scripts] name
    let out = run(&["-f", VCD, "--no-config", "run", "@nope"]);
    out.expect_code(2).expect_stderr("no script \"@nope\"");
    let out = run(&["--config", CFG, "run", "@nope"]);
    out.expect_code(2)
        .expect_stderr("no script \"@nope\"")
        .expect_stderr("scripts: cycles");
}

#[test]
fn s6_sandbox_and_budgets() {
    eval(
        r#"assert(io == nil and os == nil and debug == nil and require == nil
                  and loadfile == nil and dofile == nil)
           assert(string.format("%d", 3) == "3" and math.floor(1.5) == 1 and #"ab" == 2)
           return true"#,
    )
    .expect_code(0);

    // the instruction budget stops a runaway loop immediately
    eval_with(
        &["--format", "json", "--budget", "1000"],
        "while true do end",
    )
    .expect_code(2)
    .expect_stderr("budget");
    // 0 disables it
    eval_with(
        &["--format", "json", "--budget", "0"],
        "for i = 1, 100000 do end; return true",
    )
    .expect_code(0);

    eval_with(
        &["--format", "json", "--memory", "1"],
        "local t = {} for i = 1, 1e9 do t[i] = i end",
    )
    .expect_code(2)
    .expect_stderr("memory error");
}

#[test]
fn s7_print_goes_to_stderr_for_json_and_stdout_for_text() {
    let out = eval_with(&["--format", "json"], "print('hi'); return true");
    out.expect_code(0).expect_stderr("hi");
    assert!(
        !out.stdout.contains("hi"),
        "json stdout must stay parseable:\n{}",
        out.stdout
    );
    out.json();

    let out = eval_with(
        &["--format", "jsonl"],
        "print('hi'); wave.emit{a = 1}; return true",
    );
    out.expect_code(0).expect_stderr("hi");
    assert_eq!(out.stdout.lines().count(), 1);

    let out = eval_with(&["--format", "text"], "print('hi', 3); return true");
    out.expect_code(0);
    assert!(out.stdout.starts_with("hi\t3\n"), "got:\n{}", out.stdout);
    assert_eq!(out.stderr, "");

    // wave.note follows --quiet, print does not
    eval_with(&["--format", "text"], "wave.note('note-here'); return true")
        .expect_code(0)
        .expect_stderr("note-here");
    let out = eval_with(
        &["--format", "text", "--quiet"],
        "wave.note('note-here'); return true",
    );
    out.expect_code(0);
    assert_eq!(out.stderr, "");
}

// ---------------------------------------------------------------------------
// S5: the JSON escape hatch
// ---------------------------------------------------------------------------

#[test]
fn s8_json_escape_hatch() {
    let out = eval(&format!(
        r#"
local y = wave.signal("tb_top.ref_y")
local golden = wave.read_json("{GOLDEN}")
assert(#golden == 2)
local m = 0
for _, row in ipairs(golden) do
  if y:int(row.cycle) ~= row.y then m = m + 1 end
end
local nested = {{list = {{1, 2}}, obj = {{a = true, b = "x"}}}}
local again = wave.decode_json(wave.encode_json(nested))
assert(again.list[1] == 1 and again.list[2] == 2)
assert(again.obj.a == true and again.obj.b == "x")
-- key order in a Lua table is not stable, so compare against the text
local text = wave.encode_json(again.obj)
assert(text:find('"a":true', 1, true) and text:find('"b":"x"', 1, true))
wave.emit{{mismatches = m}}
return true
"#
    ));
    out.expect_code(0);
    let rows = rows(&out.json());
    assert_eq!(rows[0]["mismatches"], 0);

    // a missing file is a plain error
    eval(r#"wave.read_json("no/such.json")"#)
        .expect_code(2)
        .expect_stderr("wave.read_json no/such.json");
}

// ---------------------------------------------------------------------------
// S6: emission rules and the document surface
// ---------------------------------------------------------------------------

#[test]
fn s9_record_rules_and_columns() {
    // the first emit fixes the columns (sorted keys), a later unknown key fails
    eval("wave.emit{a = 1, b = 2} wave.emit{a = 3, b = 4, c = 5}")
        .expect_code(2)
        .expect_stderr("wave.emit: unknown column \"c\" (columns: a, b)");

    // wave.columns after the first emit fails
    eval("wave.emit{a = 1} wave.columns{'a'}")
        .expect_code(2)
        .expect_stderr("wave.columns: columns are already fixed by the first wave.emit");

    // a key missing from a record is null in JSON and "-" in text
    let out = eval("wave.columns{'a', 'b'} wave.emit{a = 1}");
    out.expect_code(0);
    assert_eq!(rows(&out.json())[0], serde_json::json!({"a": 1, "b": null}));
    let out = eval_with(
        &["--format", "text"],
        "wave.columns{'a', 'b'} wave.emit{a = 1}",
    );
    out.expect_code(0).expect_stdout("a b\n1 -\n");

    // columns set the order, trailer keys land after the table
    let out = eval_with(
        &["--format", "text"],
        "wave.columns{'b', 'a'} wave.emit{a = 1, b = 2} wave.trailer('note', 'x')",
    );
    out.expect_code(0)
        .expect_stdout("b a\n2 1\n")
        .expect_stdout("note: x");
    let doc = eval("wave.emit{a = 1} wave.trailer('n', 2)").json();
    assert_eq!(doc["n"], 2);

    // nothing emitted: no table at all
    let doc = eval("return true").json();
    assert!(doc.get("rows").is_none(), "{doc}");

    // side/clock errors name the flag that would fix them
    eval("wave.signal('tb_top.q3', 'b')")
        .expect_code(2)
        .expect_stderr("no side B");
    eval("wave.signal('tb_top.q3', 'c')")
        .expect_code(2)
        .expect_stderr("side must be \"a\" or \"b\", got \"c\"");
    eval_with(&["--format", "json", "--clock", "none"], "wave.cycles()")
        .expect_code(2)
        .expect_stderr("no clock (--clock none)");
    eval("wave.signal('no.such.signal')")
        .expect_code(2)
        .expect_stderr("no signal \"no.such.signal\"");
}

// ---------------------------------------------------------------------------
// S7: the config surface, and the declarative commands are untouched
// ---------------------------------------------------------------------------

#[test]
fn s10_run_from_the_config_matches_the_file() {
    let named = run(&["--config", CFG, "run", "@cycles", "--format", "json"]);
    named.expect_code(0);
    let file = run(&["--config", CFG, "run", SCRIPT, "--format", "json"]);
    file.expect_code(0);
    assert_eq!(named.stdout, file.stdout);

    let doc = named.json();
    assert_eq!(doc["script"], "@tests/fixtures/scripts/cycles.lua");
    let rows = rows(&doc);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["value"], "038");
    assert_eq!(rows[0]["int"], 56);

    // [scripts] is listed by `config` like any other section
    let out = run(&["--config", CFG, "config", "--format", "json"]);
    out.expect_code(0);
    assert!(
        out.stdout.contains("\"scripts\""),
        "config should list [scripts]:\n{}",
        out.stdout
    );
}

#[test]
fn s11_compare_is_unchanged() {
    let out = run(&[
        "-f",
        VCD,
        "--no-config",
        "compare",
        "--map",
        "tb_top.ref_y=tb_top.q3",
        "--auto-shift",
        "--from-cycle",
        "0",
        "--to-cycle",
        "19",
    ]);
    out.expect_code(1)
        .expect_stdout("135ns")
        .expect_stdout("02c")
        .expect_stdout("02d")
        .expect_stdout("mismatches: 1");
    let doc = run(&[
        "-f",
        VCD,
        "--no-config",
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
    ])
    .json();
    let pair = &rows(&doc)[0];
    assert_eq!(pair["a_cycle"], 10);
    assert_eq!(pair["b_cycle"], 13);
    assert_eq!(pair["b_cycle_local"], 13);
    assert_eq!(doc["mismatches"], 1);
}

// ---------------------------------------------------------------------------
// S8: the bundled example scripts (scripts/, documented in SCRIPTING.md §6.12)
// ---------------------------------------------------------------------------

/// Every example runs with the command line its header comment documents.
fn example(script: &str, dump: &str) -> Out {
    run(&["-f", dump, "--no-config", "run", script, "--format", "json"])
}

#[test]
fn s12_example_counter_increment() {
    // a clean invariant: no rows at all, and the exit code says "ok"
    let out = example("scripts/counter_increment.lua", GHW);
    out.expect_code(0);
    let doc = out.json();
    assert!(doc.get("rows").is_none(), "no findings -> no table: {doc}");
    assert_eq!(doc["signal"], "counter_tb.cnt");
    assert_eq!(doc["cycles_checked"], 20);
    assert_eq!(doc["increments"], 19);
    assert_eq!(doc["anomalies"], 0);
}

#[test]
fn s13_example_xz_audit() {
    let out = example("scripts/xz_audit.lua", XZ);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(
        rows(&doc),
        vec![serde_json::json!({
            "signal": "tb.v",
            "width": 4,
            "xz_changes": 1,
            "first_cycle": 0,
            "bits": "00xz",
        })]
    );
    assert_eq!(doc["signals_audited"], 2);
    assert_eq!(doc["signals_with_xz"], 1);
    assert_eq!(doc["xz_changes"], 1);
}

#[test]
fn s14_example_pair_scopes() {
    let out = example("scripts/pair_scopes.lua", TWO);
    out.expect_code(1);
    let doc = out.json();
    let got: Vec<(String, String, i64, i64, String, String)> = rows(&doc)
        .iter()
        .map(|r| {
            (
                r["a_signal"].as_str().unwrap().to_string(),
                r["b_signal"].as_str().unwrap().to_string(),
                r["mismatches"].as_i64().unwrap(),
                r["first_cycle"].as_i64().unwrap(),
                r["a"].as_str().unwrap().to_string(),
                r["b"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "tb.dut.sum".into(),
                "tb.ref.sum".into(),
                1,
                5,
                "010".into(),
                "00f".into()
            ),
            (
                "tb.dut.lane1.x".into(),
                "tb.ref.lane1.x".into(),
                1,
                6,
                "9".into(),
                "0".into()
            ),
        ]
    );
    // the same pairing the CLI's --pair-scopes finds (5 pairs, 2 divergences)
    assert_eq!(doc["pairs"], 5);
    assert_eq!(doc["unpaired"], 1);
    assert_eq!(doc["divergent_pairs"], 2);
    assert_eq!(doc["mismatches"], 2);
}

#[test]
fn s15_example_expr_check() {
    let out = example("scripts/expr_check.lua", VCD);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(
        rows(&doc),
        vec![serde_json::json!({
            "cycle": 13,
            "expected": 44,
            "actual": 45,
            "inputs": "11+33",
        })]
    );
    assert_eq!(doc["output"], "tb_top.q3");
    assert_eq!(doc["latency"], 3);
    assert_eq!(doc["checked"], 14);
    assert_eq!(doc["mismatches"], 1);
    assert_eq!(doc["first_divergent_cycle"], 13);
}

#[test]
fn s16_example_shift_scan() {
    let out = example("scripts/shift_scan.lua", VCD);
    out.expect_code(1);
    let doc = out.json();
    let scanned = rows(&doc);
    assert_eq!(scanned.len(), 9);
    // the per-shift counts `align` reports for the same window
    let counts: Vec<(i64, i64, i64)> = scanned
        .iter()
        .map(|r| {
            (
                r["shift"].as_i64().unwrap(),
                r["compared"].as_i64().unwrap(),
                r["mismatches"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(counts[0], (0, 20, 20));
    assert_eq!(counts[3], (3, 17, 1));
    assert_eq!(counts[8], (8, 12, 12));
    assert_eq!(scanned[3]["first_cycle"], 13);
    assert_eq!(doc["a"], "tb_top.ref_y");
    assert_eq!(doc["b"], "tb_top.q3");
    assert_eq!(doc["best_shift"], 3);
    assert_eq!(doc["best_mismatches"], 1);
    assert_eq!(doc["best_first_cycle"], 13);
}

#[test]
fn s17_example_transfer_scoreboard() {
    let out = example("scripts/transfer_scoreboard.lua", VCD);
    out.expect_code(1);
    let doc = out.json();
    assert_eq!(
        rows(&doc),
        vec![serde_json::json!({
            "transfer": 10,
            "a_cycle": 10,
            "b_cycle": 13,
            "a": "000101100",
            "b": "000101101",
        })]
    );
    // same event counts as `compare --align transfers` on this dump
    assert_eq!(doc["a_events"], 34);
    assert_eq!(doc["b_events"], 31);
    assert_eq!(doc["unpaired"], 3);
    assert_eq!(doc["mismatches"], 1);
}
