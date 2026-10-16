# waveq

CLI waveform-query tool for VCD/FST/GHW dumps: clock cycles, values, change
lists, per-cycle tables and golden-vs-pipelined divergence hunting — for humans
and for debugging agents.

Parsing is delegated to [`wellen`](https://crates.io/crates/wellen).
Format conversion recipes (Vivado/ModelSim/GHDL) and the full query language are
in [SKILL.md](SKILL.md).

## Build and install

```sh
cargo build --release            # target/release/waveq
cargo install --path .           # or: installs `waveq` into ~/.cargo/bin
cargo test                       # unit + end-to-end tests, no EDA tools needed
```

Requires Rust 1.88+ (edition 2024) and a C compiler: script mode vendors and
builds Lua 5.4. Only fixture *regeneration* needs EDA tools
(`tests/fixtures/gen.sh` uses `iverilog`, `vvp`, `vcd2fst`, `ghdl`); the
generated dumps are committed under `tests/fixtures/data/`.

## Commands

| command | purpose |
|---|---|
| `info` | format, version, timescale, scope/signal counts, tick range, clock |
| `scopes` | scope tree with direct counts |
| `signals` | signal list: width, type, direction, encoding, signedness |
| `signal NAME` | one signal: encoding, change statistics, first/last change |
| `clock` | resolved clock: edges, period, regularity, candidates |
| `config` | effective configuration plus resolved signals |
| `value` | value(s) at a time or cycle |
| `changes` | value-change list per signal, with cycles and deltas |
| `table` | per-cycle table of sampled signals |
| `align` | shift scan: mismatch count per candidate shift |
| `compare` | compare pairs, report every divergent cycle or transfer |
| `cut` | write a cycle window of a dump as a small VCD |
| `run FILE\|@NAME` | run a Lua script against the dump |
| `eval CODE` | run inline Lua against the dump |

Run `waveq --help` or `waveq <command> --help` for the options.

## Worked example

The repository ships a fixture: a combinational golden reference `ref_y`, a
3-stage pipeline `q1`/`q2`/`q3`, a clock-enable stall and one injected bug
(`tests/fixtures/src/pipeline.sv`).

```sh
F=tests/fixtures/data/pipeline.vcd
waveq -f $F clock
# clock: tb_top.clk (posedge edge, 46 edges, 5ns (tick 5000)..455ns (tick 455000), period 10ns (tick 10000), regular, detected by name)
waveq -f $F align --a tb_top.ref_y --b tb_top.q3 --shifts 0..=8 --from-cycle 0 --to-cycle 19
# shift 3 -> compared 17, mismatches 1, first_divergent_cycle 13; best_shift: 3, ambiguous: false
waveq -f $F compare --map tb_top.ref_y=tb_top.q3 --auto-shift --from-cycle 0 --to-cycle 19
# cycle 13  time 135ns  a_cycle 10  b_cycle 13  a 02c  b 02d  cmp mismatch
waveq -f $F table --at-cycle 13 --context 2 --signal tb_top.ref_y,tb_top.q1,tb_top.q2,tb_top.q3 --radix dec
# 13    135ns 135000 56 52 48 45   (the DUT output is wrong at cycle 13; cycle 14 is correct again)
```

With the same dump under a clock-enable stall, cycle-space comparison needs the
right shift (and degrades while the pipeline refills), while the transfer-indexed
view stays exact and reports the accepted inputs that have no output event yet:

```sh
waveq -f $F compare --align transfers --in-valid tb_top.in_valid --out-valid tb_top.out_go --map tb_top.ref_y=tb_top.q3
# transfer 10  a_cycle 10  b_cycle 13   ...  a_events 34 b_events 31 unpaired 3   (exit 1)
waveq -f $F compare --align transfers --in-valid tb_top.in_valid --out-valid tb_top.out_go \
  --map tb_top.ref_y=tb_top.q3 --from-cycle 0 --to-cycle 12
# mismatches 0, unpaired 3, exit 0  (unpaired alone is not a failure)
```

## Composition debugging (one dump, two scopes)

For a golden model that is a *composition* of the same blocks as the DUT (one
dump, `dut.*` and `ref.*`, same leaf names), pair every stage at once and read
the `first_divergent_cycle` column:

```sh
waveq -f /tmp/fir.dump.vcd signals --tree --match 'dut.*'   # scope structure
time waveq -f /tmp/fir.dump.vcd compare --pair-scopes dut,ref --shift 0
# 41 pairs, sorted by first_divergent_cycle; --delta adds `delta`/`bits_differ`,
# `--max-delta N` compares against a documented bound instead of exact equality
```

Other sides for the comparison: a second dump (`--b-file`, with `--b-clock`,
`--b-anchor`, `--b-cycle-offset` for a different preamble) or a file of golden
values (`--map dut.dout=@golden.csv:cycle,dout`).

## Cutting a failing dump down

```sh
waveq -f big.vcd cut --cycles 2040..2100 --signal 'dut.*,ref.*,tb.ce,tb.rst' -o small.vcd
# 1.97 MB -> 52 KB, same values, same cycle numbers (the clock is kept on purpose)
```

## Scripting (`run` / `eval`)

Everything above is declarative: the question has to exist as a flag already.
For the questions that do not — "first cycle where `out != f(inputs[-3:])`", a
hand-rolled scoreboard, a sweep over a knob — `run`/`eval` execute a Lua 5.4
script against the loaded dump through a small frozen `wave` API and emit rows
through the same document printer:

```sh
waveq -f $F eval 'local y = wave.signal("tb_top.ref_y")
  wave.emit{cycle = 13, value = y:value(13), int = y:int(13)}
  return y:int(13) == 56'
# {"rows": [{"cycle": 13, "value": "038", "int": 56}]}
```

* `run FILE` runs a script file, `run @NAME` a `[scripts]` entry, `eval CODE`
  inline code. `--budget` (default 50M instructions) and `--memory` (default
  256 MiB; `0` disables either) bound the script.
* The script returns `nil`/`true` for exit 0 and `false` for exit 1, so a script
  can be a check; any other return value or a script error exits 2.
* Scripts are untrusted: no `io`/`os`/`debug`/`package`, and `print` writes to
  stderr unless `--format text`, so `json`/`jsonl` output stays parseable.
* `waveq eval --help` lists the API (`wave.signal`/`:at`/`:bits`/`:int`/
  `:changes`, `wave.signals`, `wave.emit`/`wave.columns`/`wave.trailer`,
  `wave.clock`/`wave.cycles`/`wave.tick`/`wave.time`, `wave.read_json`).
  Cycle arguments are logical cycles, and `--b-*` give a script two sides.

[SCRIPTING.md](SCRIPTING.md) is the extended guide (API reference, semantics,
recipes, performance, limits) and [`scripts/`](scripts) holds six runnable
examples with their expected results.

## Configuration

`.waveq.toml` (discovered from the working directory upward, or `--config PATH`)
carries the wave file, clock, per-signal signedness/radix, named signal groups,
named checks and named table queries. See [SKILL.md](SKILL.md#config-file-waveqtoml)
for the schema and the precedence rule (CLI flag → named entry → section default
→ built-in default).

## Exit codes

* `0` — success, no divergence
* `1` — `compare`/`align` found a divergence, or a script returned `false`
* `2` — usage, IO, parse, config, clock or script error (message on stderr)

## Big dumps

VCD is what most tools emit, but FST reads several times faster and is far
smaller, and waveq treats them identically:

```sh
vcd2fst dump.vcd dump.fst     # installed at ~/.local/bin/vcd2fst
waveq -f dump.fst compare --pair-scopes dut,ref --shift 0
```

A 2 MB / 4036-cycle dump shrinks to 55 kB; `waveq info --full` then reads the
body in ~0.01 s instead of 0.05 s.

## Fixtures

`tests/fixtures/data/` holds committed VCD/FST/GHW dumps plus a hand-written
x/z VCD; regenerate the tool-generated ones with `tests/fixtures/gen.sh`.

## Layout

```
src/format.rs    radix rendering, exact tick/time arithmetic, text tables
src/resolve.rs   signal specs, glob filters, signedness/radix precedence
src/sample.rs    post-edge sampling at tick/cycle
src/clock.rs     clock detection, edge extraction, cycle map
src/load.rs      wellen loading + user-facing error mapping
src/config.rs    TOML config: discovery, parsing, precedence helpers
src/output.rs    text/json/jsonl document model
src/sides.rs     side A/B views: per-side clock, anchor and cycle offset
src/expect.rs    expected-value files (CSV/JSONL) as a comparison side
src/vcd.rs       minimal VCD writer used by `cut`
src/script/      script mode: sandboxed Lua 5.4, the `wave` API, budgets
src/cmd/         info, scopes, signals, signal, clock, config, value, changes, table, align, compare, cut, run, eval
scripts/         example scripts for run/eval (see SCRIPTING.md)
tests/           end-to-end CLI, cut and script tests
```
