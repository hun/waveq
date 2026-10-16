# waveq — waveform queries for RTL debugging agents

`waveq` answers questions about VCD/FST/GHW dumps from the command line so you
never have to write an ad-hoc parser. Parsing is done by
[`wellen`](https://crates.io/crates/wellen) (VCD, FST, GHW); bit-split VCD
vectors are merged by wellen into derived signals.

**Not supported:** Vivado WDB/WLF and Verdi FSDB (closed formats). See
[format conversion](#format-conversion) for `open_vcd`/`log_vcd` recipes.

## Conventions you must know before interpreting output

* **Cycle numbering**: cycle `c` is the tick of the `c`-th resolved clock edge
  (default `posedge`), cycle 0 = first edge in the file. **Sampling is
  post-edge**: a register that loads exactly at the edge is visible in that
  cycle. A tick before the first edge has no cycle (`-` in text, `null` in
  JSON).
* **Times**: `<N><unit>` (`10ns`, `1.5us` is rejected — only integers) converts
  exactly to ticks; a bare integer is a tick count. Conversions that are not an
  integral number of ticks are errors, never rounded. Output times use the
  largest unit that divides exactly (`135ns`, not `135000ps`).
* **Values**: `--radix` (default `auto` = `bin` for 1-bit, `hex` otherwise).
  Every JSON value record carries the canonical `bits` string as well as the
  rendered `value`, so no information is lost to radix choice. `dec` requires
  width ≤ 64 and renders `x` for any `x`/`z` bit.
* **Signedness**: VCD/FST carry none, so `reg signed [7:0]` is *unsigned*
  unless you say otherwise. Precedence: `--signed-sig`/`--unsigned-sig` >
  bare `--signed`/`--unsigned` > config `[types]` > dumped type info
  (`integer`, VHDL `signed`/`unsigned`) > unsigned. Use `signals` (column
  `signed`) or `signal` (`signed_source`) to see what was applied.
* **Divergence cycles** are reported in the **DUT (B-side) cycle** where the
  wrong value is observed; the A-side cycle is in `a_cycle`/`b_cycle` columns.
  With `compare --map A=B:3`, a divergence is reported at `b_cycle = a_cycle+3`.
* **Exit codes**: `0` no divergence / success, `1` divergence found, `2`
  usage, IO, parse, config or clock error. `unpaired` transfers never cause a
  non-zero exit by themselves.

## The golden-vs-pipelined workflow

1. **Find the two signals.**
   `waveq -f dump.vcd signals --match 'tb_top.*' --match '*y*'`
   → `full_name width var_type direction encoding signed index` per signal.
2. **Establish the cycle base.**
   `waveq -f dump.vcd clock`
   → `clock: tb_top.clk (posedge edge, 46 edges, 5ns (tick 5000)..455ns (tick 455000), period 10ns (tick 10000), regular, detected by name)`
   Check `regular: true`; if the detected clock is wrong, pass `--clock NAME`.
3. **Discover the pipeline latency** with a shift scan, restricted to a window
   where the design is not stalled.
   `waveq -f dump.vcd align --a tb_top.ref_y --b tb_top.q3 --shifts 0..=8 --from-cycle 0 --to-cycle 19`
   → `best_shift: 3`, `best_mismatches: 1`, `ambiguous: false`
4. **Compare at that shift** and read the first divergence.
   `waveq -f dump.vcd compare --map tb_top.ref_y=tb_top.q3 --auto-shift --from-cycle 0 --to-cycle 19`
   → `cycle 13  time 135ns  a_cycle 10  b_cycle 13  a 02c  b 02d  cmp mismatch`
5. **Add context at the divergence.**
   `waveq -f dump.vcd compare --map tb_top.ref_y=tb_top.q3:3 --probe tb_top.a,tb_top.stall`
   → the first row is the divergent cycle with the input values that drove the
   expected value (`probe_tb_top.a 0b`, `probe_tb_top.stall 0`).
   Then `waveq -f dump.vcd table --at-cycle 13 --context 2 --signal tb_top.ref_y,tb_top.q1,tb_top.q2,tb_top.q3`
   → `13 135ns 038 034 030 02d` + surrounding cycles.
6. **Localise the failing stage** by comparing every stage against the
   reference with the stage's own latency (`--map` is repeatable; the pairs are
   merged into one sorted divergence list).
   `waveq -f dump.vcd compare --map tb_top.ref_y=tb_top.q1:1 --map tb_top.ref_y=tb_top.q2:2 --map tb_top.ref_y=tb_top.q3:3 --from-cycle 0 --to-cycle 19`
   → `pair 0 (q1, shift 1) status ok`; `pair 1 (q2, shift 2) divergent, first_divergent_cycle 12`;
   `pair 2 (q3, shift 3) divergent, first_divergent_cycle 13`.
   The earliest divergent stage (here `q2` at cycle 12, the stage with the
   injected fault) is the culprit; later stages only show the propagated effect.
7. **When a stall/enable exists, use transfers mode.** A global clock enable
   makes a fixed cycle shift wrong while the pipeline refills; only the
   valid/ready-gated event streams stay comparable.
   `waveq -f dump.vcd compare --align transfers --in-valid tb_top.in_valid --out-valid tb_top.out_go --map tb_top.ref_y=tb_top.q3`
   → `transfer 10  a_cycle 10  b_cycle 13`, summary `a_events 34 b_events 31 paired 31 unpaired 3`,
   exit code 1.
   `unpaired` equals the pipeline depth for a correct design (the last accepted
   inputs have no output event yet). If `unpaired` differs between two runs of
   the same design, a transfer was swallowed or duplicated — that is a real
   bug even when every compared value matches.
   Pass the **gated** output valid (or a `--out-ready`): a held output is not a
   new transfer, so counting `ov` alone during a stall produces phantom
   divergences.

## Commands

Global options work before or after the subcommand:
`-f/--file PATH`, `--config PATH`, `--no-config`, `--clock NAME|none`,
`--edge posedge|negedge|both`, `--radix auto|bin|hex|oct|dec`, `--signed`,
`--unsigned`, `--signed-sig NAME`, `--unsigned-sig NAME`,
`--format text|json|jsonl`, `--max-rows N`, `--quiet`.

`--format text` prints `key: value` header lines and one aligned table;
`json` prints one document; `jsonl` prints one object per line, starting with a
`{"record":"header",...}` line for commands that have a header block
(`align`, `compare`) and ending with `{"record":"footer",...}` when there are
summary fields. Named tables (`pairs`, `candidates`, `resolved`, …) appear
under their own key/record name.

| command | purpose |
|---|---|
| `info` | file format, version, timescale, counts, tick range |
| `scopes` | scope tree with direct var/scope counts |
| `signals` | signal list with width, type, direction, signedness |
| `signal NAME` | one signal: encoding, change statistics, first/last change |
| `clock` | resolved clock: edges, period, regularity, candidates |
| `config` | effective config after discovery/merge, plus resolved signals |
| `value` | value(s) at a time or cycle |
| `changes` | value-change list per signal |
| `table` | per-cycle table of sampled signals |
| `align` | shift scan: mismatches per candidate shift |
| `compare` | compare pairs, report every divergent cycle/transfer |

### info

```
waveq -f dump.vcd info --full --with-clock
file: dump.vcd
format: vcd
timescale: 1ps
scopes: 2
vars: 20
tick_count: 93
first_tick: 0
first_time: 0s
last_tick: 460000
last_time: 460ns
clock: tb_top.clk (posedge edge, 46 edges, 5ns (tick 5000)..455ns (tick 455000), period 10ns (tick 10000), regular, detected by name)
```

`--full` reads the body (tick range); `--with-clock` adds the clock summary
(named `--with-clock` because `--clock` is the global clock selector).

### scopes

```
waveq -f dump.vcd scopes --depth 0
full_name depth<=0 type   vars scopes
tb_top    0        module 17   1
```

`--depth N` limits depth (top-level scopes are depth 0), `--match GLOB` filters
full names, `--max-rows` caps rows (`truncated: true` is reported).

### signals

```
waveq -f dump.vcd signals --match 'tb_top.q*' --scope tb_top
full_name width var_type direction encoding     signed index
tb_top.q1 9     reg      unknown   bitvector(9) false  8:0
tb_top.q2 9     reg      unknown   bitvector(9) false  8:0
tb_top.q3 9     reg      unknown   bitvector(9) false  8:0
```

`--count` prints only `count: N`. `twos/complement` types are not invented: use
`--signed` or `[types]`.

### signal

```
waveq -f dump.vcd signal tb_top.q3
full_name: tb_top.q3
width: 9
changes: 32
distinct_values: 32
transitions: 31
first_change: 0s (tick 0, cycle -, 000)
last_change: 455ns (tick 455000, cycle 45, 07c)
signed_source: inferred
```

`--bits 7:4` inspects a slice.

### clock

```
waveq -f dump.vcd clock
clock: tb_top.clk (posedge edge, 46 edges, 5ns (tick 5000)..455ns (tick 455000), period 10ns (tick 10000), regular, detected by name)
```

Detection: 1-bit signals whose leaf name contains `clk`/`clock` and that have
≥ 4 edges, ranked by edge count. With no name match it falls back to any 1-bit
signal with ≥ 8 edges and prints a note; if that also fails it errors and lists
the 1-bit signals by edge count. `--clock none` is accepted by `changes` and
`table` (tick/time-only output) and is rejected by the cycle-indexed commands.
`--list` prints only the candidate table.

### config

```
waveq -f dump.vcd --config .waveq.toml config
config: .waveq.toml
file: sim/dump.fst
key            signal      width signed radix source
types.tb_top.sn tb_top.sn  8     true   dec   config
wave.clock     tb_top.clk  1     false  auto  inferred
resolved: 3 signals
```

Without a wave file the merged config is printed with `resolved: null`.
Unresolvable names are errors with suggestions.

### value

```
waveq -f dump.vcd value --cycle 13 --signal tb_top.ref_y,tb_top.q3
signal       cycle tick   time  value bits
tb_top.ref_y 13    135000 135ns 038   000111000
tb_top.q3    13    135000 135ns 02d   000101101
```

`--at <TIME|tick>` or `--cycle N` (exactly one). `--at` also reports the cycle
when the clock can be resolved; a tick before the first edge gives `cycle -`.

### changes

```
waveq -f dump.vcd changes --signal tb_top.ref_y --from-cycle 0 --to-cycle 3
cycle time signal       value bits
0     5ns  tb_top.ref_y 004   000000100
1     15ns tb_top.ref_y 008   000001000
2     25ns tb_top.ref_y 00c   000001100
3     35ns tb_top.ref_y 010   000010000
```

Range flags: `--from-cycle/--to-cycle`, `--from/--to` (time/tick), `--delta`
(adds `delta_cycles`), `--no-initial` (drops each signal's initial value),
`--at-edges-only` (keeps changes exactly at a clock edge — the synchronous
view). Bounds that are pre-edge are simply excluded (rows keep
`"pre_clock": true`).

### table

```
waveq -f dump.vcd table --at-cycle 13 --context 2 --signal tb_top.ref_y,tb_top.q1,tb_top.q2,tb_top.q3
cycle time  tick   tb_top.ref_y tb_top.q1 tb_top.q2 tb_top.q3
11    115ns 115000 030          02c       028       024
12    125ns 125000 034          030       02d       028
13    135ns 135000 038          034       030       02d
14    145ns 145000 03c          038       034       030
15    155ns 155000 040          03c       038       034
```

Ranges: `--cycles 11..15`, `--from-cycle/--to-cycle`, `--at-cycle N --context K`,
`--time 125ns..145ns`, or `--query NAME` from `[queries]`. The default range is
the whole dump, capped by `--max-rows` (default 50) with `truncated: true`.
With `--clock none` rows are time-table ticks instead of cycles.

### align

```
waveq -f dump.vcd align --a tb_top.ref_y --b tb_top.q3 --shifts 0..=8 --from-cycle 0 --to-cycle 19
shift compared mismatches first_divergent_cycle first_divergent_time
0     20       20         0                     5ns      # 8 shifts scanned; only two shown
3     17       1          13                    135ns
best_shift: 3
best_mismatches: 1
ambiguous: false
```

Cycles mode compares `a[c]` with `b[c+shift]` for `c` inside the window
(`--from-cycle/--to-cycle`, so both indices stay in range). `--gate NAME`
restricts compared cycles to those where the gate is `1`.
Transfers mode (`--align transfers --in-valid X [--in-ready Y] --out-valid Z
[--out-ready W]`) compares event `k` of the A stream with event `k+shift` of the
B stream; `--gate` is rejected there. `best_shift` = smallest shift with the
fewest mismatches; `ambiguous: true` means several shifts align equally well, so
the latency is not identifiable from this pair. Exit `0` when
`best_mismatches == 0`, else `1`.

### compare

```
waveq -f dump.vcd compare --map tb_top.ref_y=tb_top.q3 --auto-shift --from-cycle 0 --to-cycle 19
cycle time  a_cycle b_cycle pair a   b   cmp      a_at b_at
13    135ns 10      13      0    02c 02d mismatch 55ns 55ns
pairs:
pair a            b         shift align  compared mismatches x_involved skipped first_divergent_cycle first_divergent_time status
0    tb_top.ref_y tb_top.q3 3     cycles 17       1          0          0       13                    135ns                divergent
```

* `--map A=B[:SHIFT]` is repeatable; `--check NAME` uses a `[checks]` entry;
  CLI `--map`s are *appended* to a check's pair, and any CLI flag overrides the
  check's field (including `--shift`).
* In cycles mode a pair **must** have a shift (`A=B:3`, the check entry, or
  `--auto-shift`). Without it the command errors: a silent 0 would compare
  pipelined values against each other as if the latency were zero.
* `--cmp bit-exact` (default) compares canonical bit strings (equal widths
  required — use `--cmp numeric` for width-agnostic comparison);
  `--cmp numeric` interprets each side with its own signedness and width and
  compares as an integer (reals/strings are never coerced).
* `--x mismatch` (default) counts an `x`/`z`-involved unit as a divergence
  labeled `x-involved`; `--x wildcard` treats it as neither a match nor a
  divergence but still counts `x_involved`.
* `--probe NAME` adds a column per signal sampled at the row's A-side cycle
  (the cycle that produced the expected value); `--at-edges-only` keeps only
  rows whose values changed exactly at an edge; `--first-only`/`--max-rows`
  (default 20) trim rows.

## Config file (`.waveq.toml`)

Discovered from the CWD upward (stopping at the first ancestor containing
`.git`), or given with `--config`; `--no-config` disables discovery. When a
discovered config is used, `using config PATH` is printed to stderr
(`--quiet` suppresses it).

```toml
[wave]
file   = "sim/dump.fst"      # default for -f
clock  = "tb_top.clk"        # default for --clock
edge   = "posedge"           # posedge|negedge|both

[compare]                    # defaults for align/compare
align     = "cycles"         # cycles|transfers
cmp       = "bit-exact"      # bit-exact|numeric
x         = "mismatch"       # mismatch|wildcard
max_shift = 16
in_valid  = "tb_top.in_valid"   # transfers mode
in_ready  = "tb_top.in_ready"   # optional
out_valid = "tb_top.out_go"     # transfers mode
out_ready = "tb_top.out_ready"  # optional
gate      = "tb_top.in_valid"   # cycles mode

[types]                      # rendering/signedness overrides (specs or globs)
"tb_top.sn"        = { signed = true, radix = "dec" }
"tb_top.q*"        = { radix = "hex" }
"tb_top.bus[15:8]" = { radix = "bin" }

[signals]                    # named groups: --signal @inputs
inputs = ["tb_top.a", "tb_top.b", "tb_top.in_valid"]

[checks]                     # named compare invocations: --check out_delay
out_delay = { a = "tb_top.ref_y", b = "tb_top.q3", shift = 3, from_cycle = 0, to_cycle = 19 }

[queries]                    # named table invocations: --query pipeline
pipeline = { signals = ["tb_top.ref_y", "tb_top.q1"], context = 3 }
```

Unknown keys are errors (`unknown config key "x"`). `[checks]` accepts every
`compare` option (`a`, `b`, `shift`, `auto_shift`, `max_shift`, `align`, `cmp`,
`x`, `gate`, `in_valid`, `in_ready`, `out_valid`, `out_ready`, `from_cycle`,
`to_cycle`, `from`, `to`, `probe`, `at_edges_only`); `[queries]` accepts
`signals`, `context`, `cycles`, `from_cycle`, `to_cycle`, `at_cycle`.
`[types]` keys are exact signal specs (winner) or globs (first match in file
order).

**Precedence, highest first:** explicit CLI option → named entry
(`--check`/`--query`/`@group`) → config section default → built-in default.

## Failure modes and how to tell them apart

| symptom | cause | query that distinguishes it |
|---|---|---|
| Everything mismatches, `a_at`/`b_at` constant | wrong clock or wrong edge | `clock`; then `--clock NAME` / `--edge negedge` |
| Values lag by one cycle | the design dumps register updates 1ps after the edge, and you sampled pre-edge... post-edge sampling is the default | `compare` row `a_change_time`/`b_change_time` right after the edge tick; then `--edge` or shift by one |
| Exactly one extra cycle of latency | correct pipeline depth discovered wrongly | `align --shifts 0..=8` and check `ambiguous` |
| Mismatch only after a stall | fixed shift used across a clock-enable stall | `align` unbounded vs a pre-stall window, or `--align transfers` |
| Mismatch count changes with `--from/--to` | the window excludes the interesting region | `compare --from-cycle 0 --to-cycle N` and watch `first_divergent_cycle` |
| `cmp: x-involved` | unknown bits in the dump (uninitialised memory, no reset) | `value --at T --signal S --radix bin`, or `--x wildcard` to ignore |
| Values that look 2×/256× off, negatives shown as big positives | wrong radix/signedness | `signals` column `signed`, `signal` field `signed_source` |
| `unpaired` differs between two runs | swallowed or duplicated transfer | transfers-mode summary `a_events`/`b_events`/`unpaired` |
| `no clock found` | no name match and no busy 1-bit signal | `clock` candidate list, then `--clock NAME` or `[wave] clock` |

## Format conversion

```
# Vivado / XSim, in the Tcl console:
open_vcd dump.vcd ; log_vcd * ; run all ; close_vcd
# ModelSim / Questa:
vcd file dump.vcd ; vcd add -r /tb/* ; run -all
# Icarus Verilog:  $dumpfile("dump.vcd"); $dumpvars(0, tb);
# GHDL:            ghdl -r tb --wave=dump.ghw --stop-time=1us
# Verilator:       --trace (VCD) / --trace-fst (FST)
# vcd2fst dump.vcd dump.fst        # smaller and much faster to read
# gtkwave/surfer can export VCD from many sources
```

WDB, WLF (Vivado/ModelSim native) and FSDB (Verdi) are closed formats and are
**not** read by waveq; dump to VCD/FST/GHW instead. Bounded windows help: a
10 GB FST is slow to read even when only 20 cycles matter — use
`log_vcd`/`vcd add` windows, or `--max-rows 0` only when needed.

## Notes for agents

* Prefer `--format jsonl` for streaming: `header`, then `row`s, then `footer`.
* Cycle-indexed numbers are only meaningful together with the clock they came
  from; `align`/`compare`/`table` echo `clock`/`edge` in their header.
* `--max-rows 0` means unlimited; the default caps are `signals`/`scopes` 500,
  `changes`/`table` 50, `compare` 20, `align` 50.
* `--quiet` silences notes (`using config`, truncation, clock fallback), never
  errors. Errors always go to stderr and exit with code 2.
