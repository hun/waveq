# Scripting waveq with Lua

The declarative commands (`table`, `changes`, `align`, `compare`, …) answer the
questions that already have a flag. `waveq run` and `waveq eval` cover the rest:
they execute a Lua 5.4 script against the loaded dump through a small frozen
`wave` API, and the rows it emits go through the normal document printer — so
`--format text|json|jsonl` and the 0/1/2 exit-code contract keep their meaning.

```sh
waveq -f dump.vcd eval 'local y = wave.signal("tb_top.ref_y")
  wave.emit{cycle = 13, value = y:value(13), int = y:int(13)}
  return y:int(13) == 56'
```

```
file: dump.vcd
script: =eval
sides: a
cycle int value
13    56  038
```

Script mode is a third way to ask a question, not a new query language: it can
only do what the `wave` API exposes, and everything it can reach is listed by
`waveq eval --help` and in [§4](#4-api-reference). `tests/script.rs` is the
executable specification of the surface.

## Contents

1. [When to write a script instead of a command](#1-when-to-write-a-script-instead-of-a-command)
2. [Running a script](#2-running-a-script)
3. [The output model](#3-the-output-model)
4. [API reference](#4-api-reference)
5. [Semantics you must get right](#5-semantics-you-must-get-right)
6. [Recipes](#6-recipes)
7. [Performance](#7-performance)
8. [Errors, debugging, and scripts as checks](#8-errors-debugging-and-scripts-as-checks)
9. [Limits and sharp edges](#9-limits-and-sharp-edges)

## 1. When to write a script instead of a command

| question | how to ask it |
|---|---|
| "what is ref_y at cycle 13?" | `value`, `table` |
| "where do ref_y and q3 differ, at which shift?" | `align`, `compare --auto-shift` |
| "which pairs of `dut.*`/`ref.*` diverge?" | `compare --pair-scopes dut,ref` |
| "which accepted input never produced an output event?" | `compare --align transfers` |
| "first cycle where `q3 == a[c-3] + b[c-3]` fails, with the inputs" | **script** (a predicate the flags cannot spell) |
| "rank all shifts, then print only the top three with both values" | **script** |
| "every signal where the value dwell time exceeds N cycles" | **script** |
| "audit every x/z sample in a window and count them per signal" | **script** |
| "join a golden JSON file against the dump and summarise per row" | **script** |
| "decode a bus into fields and report the first lane that misbehaves" | **script** |

Three signals that scripting is the right tool:

* the condition involves **arithmetic or a previous value** (`f(inputs[-3:])`,
  `out == in1 + in2`, `delta > N`);
* the output is **shaped differently** from what a command produces (a ranking
  table, a per-signal summary, a nested report);
* the analysis is **iterative** (find something, then look around it: context
  cycles, the inputs that caused it, the value before it).

If a command already answers the question, prefer it: it is tested, documented,
and does not cost you a Lua dialect decision.

## 2. Running a script

```sh
waveq run FILE            # script file
waveq run @NAME           # [scripts] NAME from .waveq.toml
waveq eval 'CODE'         # inline source
```

Six working examples ship in [`scripts/`](scripts) — see
[§6.12](#612-bundled-example-scripts) for the commands and the expected results:

```sh
waveq -f tests/fixtures/data/counter.ghw --no-config run scripts/counter_increment.lua
```

### Exit codes

| script result | exit | meaning |
|---|---|---|
| returns `nil` or `true` | 0 | ok |
| returns `false` | 1 | a check failed (same class as `compare`'s divergence) |
| returns any other value | 2 | usage error: "the script returned a integer; return nil, true or false" |
| any script error | 2 | `waveq: runtime error: …` plus a stack traceback on stderr |

That makes a script usable as a check: the shell sees the same 0/1/2 it sees
from `compare`.

### Budgets

`--budget N` bounds executed VM instructions (default 50 000 000, `0` =
unlimited) and `--memory MIB` bounds the Lua state's allocations (default 256,
`0` = unlimited). Both are *subcommand* options, so they go after `run`/`eval`:

```sh
waveq -f dump.vcd eval 'while true do end' --budget 1000
# waveq: runtime error: instruction budget exceeded (--budget 1000)   (exit 2, immediately)
```

The memory limit covers Lua tables and strings only; the waveform data itself is
host-side and is bounded by the dump, not by `--memory`.

### Sandbox

An untrusted script gets the safe standard library minus the file system:
`io`, `os`, `debug`, `package`, `require`, `loadfile` and `dofile` are absent
(`load`/`loadstring` remain: they only compile strings). `string`, `math`,
`table`, `utf8`, `coroutine`, `select`, `pcall` and `print` are available.

`print` is routed so that structured output stays parseable: with
`--format text` it writes to stdout (interleaved with the document header),
with `--format json`/`jsonl` it writes to stderr. Use `wave.note(msg)` for
progress messages; it goes to stderr unless `--quiet`.

### Options that reach the script

Every global flag is resolved before the script runs, so the script sees their
effect rather than re-implementing them:

| flag | effect inside the script |
|---|---|
| `-f`, `--b-file` | the two dumps (`wave.sides()`, `wave.path(side)`) |
| `--clock`, `--b-clock`, `--edge` | the cycle maps of both sides (`wave.clock`, `wave.cycles`, all cycle arguments) |
| `--cycle-offset`, `--anchor`, `--b-anchor`, `--b-cycle-offset` | the per-side origins (logical cycle numbering) |
| `--radix`, `[types]`, `--signed`, `--unsigned-sig`, … | the default rendering/signedness of every handle (`:value`, `:int`, `radix`, `signed`) |
| `--format` | where `print` goes |
| `--quiet` | whether `wave.note` and clock notes print |
| `--budget`, `--memory` | the VM limits |

`--max-rows` is **not** applied: a script's rows are its own output, so trim
inside the script if you need a cap.

## 3. The output model

A script writes zero or more rows with `wave.emit(record)` and optional summary
fields with `wave.trailer(key, value)`; `Doc` renders them exactly like a
command's document.

```lua
wave.columns{"cycle", "signal", "value"}     -- column order (optional)
wave.emit{cycle = 13, signal = "tb_top.ref_y", value = "038"}
wave.emit{cycle = 14, signal = "tb_top.ref_y", value = "03c"}
wave.trailer("rows_reported", 2)
return true
```

```
file: dump.vcd
script: @check_dut.lua
sides: a
cycle signal       value
13    tb_top.ref_y 038
14    tb_top.ref_y 03c
rows_reported: 2
```

Rules (all of them are errors, never silent fixes):

* the argument of `wave.emit` must be a table with named columns; a function or
  userdata inside it fails (`deserialize error: unsupported value type`);
* the **first** `wave.emit` fixes the column list — from `wave.columns{…}` when
  you called it, otherwise the record's keys **sorted alphabetically**;
* a later record with an unknown key fails:
  `wave.emit: unknown column "c" (columns: a, b)`;
* a key missing from a record emits `null` in JSON and `-` in text;
* `wave.columns` after the first `wave.emit` fails:
  `wave.columns: columns are already fixed by the first wave.emit`;
* a script that emits nothing produces **no table at all** (rather than an empty
  table with guessed columns);
* values may be nested tables (arrays/objects): they become nested JSON and
  render as compact JSON in text mode;
* `null`/missing is the only way to say "no value" — use it instead of a
  sentinel string, and let `-` appear only in text output.

Header fields are added for you: `file` (side A's dump), `script` (`@path` for a
file, `=eval` for inline code) and `sides` (`a` or `ab`). `--format jsonl` emits
a `{"record":"header",…}` line only when the script returned `false` (the exit-1
case), matching the other commands.

## 4. API reference

The only global is `wave`. `side` is an optional trailing `"a"` (default) or
`"b"`; `"b"` requires a B side (`--b-file`, `--b-clock`, `--b-anchor` or
`--b-cycle-offset`).

### Functions

| call | returns |
|---|---|
| `wave.sides()` | `{"a"}` or `{"a","b"}` |
| `wave.path([side])` | dump path of that side (B falls back to A) |
| `wave.info()` | `file, format, version, date, timescale, scopes, vars, tick_count, first_tick, first_time, last_tick, last_time` (side A) |
| `wave.clock([side])` | the same fields as `clock --format json` (`signal`, `edge`, `edges`, `first_tick`, `period_median`, `regular`, …), or `nil` with `--clock none` |
| `wave.cycles([side])` | number of clock edges |
| `wave.last_cycle([side])` | highest logical cycle |
| `wave.tick(cycle, [side])` | tick of that logical cycle, `nil` outside `0..=last_cycle` |
| `wave.time(tick)` | tick as time text (`"135ns"`) |
| `wave.signal(name, [side])` | a signal handle; `name` is a spec (`NAME` or `NAME[msb:lsb]`) |
| `wave.signals([pattern], [side])` | list of records `{full_name, width, var_type, direction, encoding, signed, signed_source, index}` in hierarchy order, filtered by the glob `pattern` |
| `wave.emit(record)` | append one output row |
| `wave.columns{...}` | column order, before the first `emit` |
| `wave.trailer(key, value)` | a summary field after the table |
| `wave.note(msg)` | stderr note (suppressed by `--quiet`) |
| `wave.encode_json(value)` / `wave.decode_json(str)` | JSON text ⇄ value |
| `wave.read_json(path)` | a JSON file as a value |

### Handle fields

| field | value |
|---|---|
| `name` | full hierarchy name (`tb_top.ref_y`) |
| `spec` | the spec as written (`tb_top.ref_y[8:4]`) |
| `width` | width after slicing |
| `signed` | effective signedness (CLI flag > `[types]` > inferred) |
| `radix` | effective default radix (`auto`, `hex`, …) |
| `kind` | `bitvec`, `real` or `string` |
| `direction` | `input`/`output`/`inout`/`unknown`/… |
| `index` | declared range as text (`"8:0"`) or `nil` |

### Handle methods

| method | returns |
|---|---|
| `:at(cycle)` | `{cycle, tick, time, value, bits, has_x}` or `nil` when the signal has no value there |
| `:at_tick(tick)` | the same without `cycle` |
| `:bits(cycle)` | the canonical MSB-first bit string (`"000111000"`) or `nil` |
| `:value(cycle[, radix])` | the rendered string (`"038"`, `"56"`, `"1.5"`), or `nil`; `radix` is `auto\|bin\|hex\|oct\|dec` and defaults to the handle's |
| `:int(cycle)` | the value as a Lua integer, or `nil` for x/z and for signals with no bits |
| `:is_xz(cycle)` | `true` when any bit is `x`/`z` (`false` for `real`/`string` and for no value) |
| `:changes([from_cycle[, to_cycle]])` | `{cycle, tick, bits, value, has_x}` per value change, ascending by tick, restricted to the logical-cycle window |

`:bits` is the canonical form — use it for equality, `:int` for arithmetic,
`:value` for humans (it applies the radix and signedness), `:at` when you want
the whole sample or the `has_x` flag. For `real`/`string` signals (`kind`)
there are no bits: `:bits`/`:int` return `nil` and `:value()` renders the number
or the text.

### Errors you may see

| message | cause |
|---|---|
| `no side B: pass --b-file, --b-cycle-offset or --b-anchor` | `"b"` without a B side |
| `side must be "a" or "b", got "c"` | typo in the side argument |
| `wave.cycles: no clock (--clock none)` | a cycle-indexed call with `--clock none` |
| `invalid radix "x": use auto\|bin\|hex\|oct\|dec` | `:value(c, radix)` typo |
| `tb_top.ref_y: 18446744073709551615 does not fit a Lua integer; use :value() or :bits()` | unsigned 64-bit values above `2^63-1` |
| `decimal rendering requires width <= 64 (has 65); use --radix hex` | `:int` on a wider signal |
| `no signal "tb.q3". did you mean: …` | unknown name (the CLI's suggestion list) |

## 5. Semantics you must get right

**Cycles are logical cycles.** Cycle `c` of a side is the side's local edge
`c + origin`, where the origin folds in `--cycle-offset` and `--anchor`. All
cycle arguments (`:at`, `:bits`, `:changes`, `wave.tick`) are logical, so they
line up with the CLI's `cycle`/`a_cycle`/`b_cycle` columns. `:at(999)` is `nil`
rather than an error, and `:changes()` may report negative cycles for changes
that happened before the first edge.

**Sampling is post-edge.** A value that changes exactly at the clock edge is
visible in that cycle, matching `table`/`compare`.

**Bits, integers, rendered values.** `:bits(c)` is the canonical MSB-first
string, which is the same string `--cmp bit-exact` compares — use it for
equality. `:int(c)` is a Lua 5.4 integer (`i64`): it needs width ≤ 64, returns
`nil` when any bit is `x`/`z`, and applies the handle's signedness, so
`--signed-sig tb_top.sn` turns `253` into `-3`. `:value(c)` is only for display;
comparing rendered strings across width/radix differences is a trap.

**Side A and side B are independent views.** With `--b-cycle-offset 3`, side B's
logical cycle 0 is its local cycle 3, so `c` against `c` compares the DUT with
the goldens *including* that offset. With `--b-anchor NAME` on both sides the two
origins move together and the latency must come from an explicit shift you apply
yourself. When in doubt, print `a:at(c).cycle`/`b:at(c).cycle` and compare them
with the CLI's `a_cycle`/`b_cycle` columns.

**Cycle space is not transfer space.** With a clock-enable stall, a fixed
cycle-space offset stops meaning anything while the pipeline refills, because
cycles pass without a transfer. Anything that must be exact across a stall has to
count events (`in_valid`/`out_go` edges) instead of cycles — see
[§6.5](#65-transfer-space-scoreboard-stall-proof). The bundled fixture
(`tests/fixtures/src/pipeline.sv`) has exactly such a stall, so it is a good
place to see the difference: over the whole dump, the cycle view diverges at
cycles 10 (the injected bug) and 17, 18, 19 (the stall refill), while the
transfer view has one single mismatch.

## 6. Recipes

Every recipe below runs against the bundled fixtures:

```sh
F=tests/fixtures/data/pipeline.vcd      # 46 cycles, ref_y vs the 3-deep q3
T=tests/fixtures/data/two_scopes.vcd    # tb.dut.* vs tb.ref.*
X=tests/fixtures/data/x_values.vcd      # x/z samples
```

The output blocks below show only the table and trailer fields; the
`file`/`script`/`sides` header lines (§3) are printed too.

### 6.1 The skeleton

Explicit columns, one row per finding, summary in the trailer:

```lua
wave.columns{"cycle", "signal", "expected", "actual"}
wave.emit{cycle = 13, signal = "tb_top.ref_y", expected = "038", actual = "038"}
wave.trailer("cycles_scanned", wave.cycles())
return true
```

### 6.2 Expression checking

The canonical script-mode question: does the DUT output equal an expression over
earlier inputs? Here `q3[c]` must equal `a[c-3] + b[c-3]` (the fixture's
3-deep pipeline); the injected bug makes one cycle fail.

```lua
local q, a, b = wave.signal("tb_top.q3"), wave.signal("tb_top.a"), wave.signal("tb_top.b")
wave.columns{"cycle", "expected", "actual", "a", "b"}
local checked, mismatches, first = 0, 0, nil
for c = 3, 16 do
  local expected = a:int(c - 3) + b:int(c - 3)
  local actual = q:int(c)
  checked = checked + 1
  if actual ~= expected then
    mismatches = mismatches + 1
    if first == nil then first = c end
    wave.emit{cycle = c, expected = expected, actual = actual,
              a = a:int(c - 3), b = b:int(c - 3)}
  end
end
wave.trailer("checked", checked)
wave.trailer("mismatches", mismatches)
wave.trailer("first_divergent_cycle", first)
return mismatches == 0
```

```
cycle expected actual a  b
13    44       45     11 33
checked: 14
mismatches: 1
first_divergent_cycle: 13
```

Exit code 1: the `return mismatches == 0` turns the finding into the process
status, so a shell can gate on it. The range stops at cycle 16 on purpose — the
fixture's stall (cycles 20..31) freezes the inputs, and the expression is only
meaningful while the pipeline is fed (see §6.5 for the stall-proof version).

### 6.3 Sweeps and ranking

`align` scans one shape of shift; a script can sweep anything and rank the
results. This is the same scan, emitted as a full table:

```lua
local y, q = wave.signal("tb_top.ref_y"), wave.signal("tb_top.q3")
wave.columns{"shift", "compared", "mismatches"}
local best, worst = nil, nil
for s = 0, 8 do
  local mismatches, compared = 0, 0
  for c = s, 19 do
    compared = compared + 1
    if y:bits(c - s) ~= q:bits(c) then mismatches = mismatches + 1 end
  end
  wave.emit{shift = s, compared = compared, mismatches = mismatches}
  if best == nil or mismatches < best.mismatches then best = {s = s, mismatches = mismatches} end
  if worst == nil or mismatches > worst.mismatches then worst = {s = s, mismatches = mismatches} end
end
wave.trailer("best_shift", best.s)
wave.trailer("best_mismatches", best.mismatches)
wave.trailer("worst_shift", worst.s)
return best.mismatches == 0
```

```
shift compared mismatches
0     20       20
1     19       19
2     18       18
3     17       1
4     16       16
5     15       15
6     14       14
7     13       13
8     12       12
best_shift: 3
best_mismatches: 1
worst_shift: 0
```

(`align --a tb_top.ref_y --b tb_top.q3 --shifts 0..=8 --from-cycle 0 --to-cycle
19` reports the same per-shift counts and the same `best_shift: 3`.)

### 6.4 Gold against DUT on two sides

With `--b-cycle-offset 3` the pipeline latency is absorbed by side B's origin, so
logical cycle `c` of both sides is the same transfer — the natural way to write a
gold/DUT comparison:

```lua
local a, b = wave.signal("tb_top.ref_y"), wave.signal("tb_top.q3", "b")
wave.columns{"cycle", "a", "b"}
local mismatches = 0
for c = 0, 16 do
  if a:bits(c) ~= b:bits(c) then
    mismatches = mismatches + 1
    wave.emit{cycle = c, a = a:value(c), b = b:value(c)}
  end
end
wave.trailer("mismatches", mismatches)
return mismatches == 0
```

```sh
waveq -f $F --b-cycle-offset 3 eval '<the script above>' --format json
# rows: [{"cycle": 10, "a": "02c", "b": "02d"}], mismatches: 1
```

The CLI's equivalent (`compare --map tb_top.ref_y=tb_top.q3 --auto-shift`) calls
the same pair `a_cycle 10` / `b_cycle 13`; with the offset the script writes both
as cycle 10 because side B's origin is already 3.

### 6.5 Transfer-space scoreboard (stall-proof)

When a clock enable stalls the design, cycles pass without a transfer and any
cycle-space comparison degrades. Count transfers instead: pair the k-th accepted
input with the k-th output event, exactly like `compare --align transfers`.

```lua
local in_valid, out_go = wave.signal("tb_top.in_valid"), wave.signal("tb_top.out_go")
local y, q = wave.signal("tb_top.ref_y"), wave.signal("tb_top.q3")
local inputs, outputs = {}, {}
for c = 0, wave.last_cycle() do
  if in_valid:bits(c) == "1" then inputs[#inputs + 1] = {cycle = c, bits = y:bits(c)} end
  if out_go:bits(c) == "1" then outputs[#outputs + 1] = {cycle = c, bits = q:bits(c)} end
end
wave.columns{"transfer", "a_cycle", "b_cycle", "a", "b"}
local mismatches = 0
for k = 1, math.min(#inputs, #outputs) do
  if inputs[k].bits ~= outputs[k].bits then
    mismatches = mismatches + 1
    wave.emit{transfer = k - 1, a_cycle = inputs[k].cycle, b_cycle = outputs[k].cycle,
              a = inputs[k].bits, b = outputs[k].bits}
  end
end
wave.trailer("a_events", #inputs)
wave.trailer("b_events", #outputs)
wave.trailer("unpaired", math.abs(#inputs - #outputs))
wave.trailer("mismatches", mismatches)
return mismatches == 0
```

```
transfer a_cycle b_cycle a         b
10       10      13      000101100 000101101
a_events: 34
b_events: 31
unpaired: 3
mismatches: 1
```

Identical to the CLI's `compare --align transfers --in-valid tb_top.in_valid
--out-valid tb_top.out_go --map tb_top.ref_y=tb_top.q3`: one divergent transfer
(number 10), and `unpaired 3` is the pipeline depth — the accepted inputs whose
output event has not happened yet, which is not a failure by itself.

### 6.6 Pairing every signal across two scopes

`wave.signals(pattern)` gives you names to drive the analysis, so a script can do
what `--pair-scopes` does and then report whatever detail it wants:

```lua
wave.columns{"a_signal", "b_signal", "mismatches", "first_cycle", "a", "b"}
local pairs, total = 0, 0
for _, rec in ipairs(wave.signals("tb.dut.*")) do
  local twin = (rec.full_name:gsub("^tb%.dut%.", "tb.ref."))
  local ok_a, a = pcall(wave.signal, rec.full_name)
  local ok_b, b = pcall(wave.signal, twin)
  if ok_a and ok_b then
    pairs = pairs + 1
    local mismatches, first, av, bv = 0, nil, nil, nil
    for c = 0, wave.last_cycle() do
      if a:bits(c) ~= b:bits(c) then
        mismatches = mismatches + 1
        if first == nil then first, av, bv = c, a:value(c), b:value(c) end
      end
    end
    total = total + mismatches
    if mismatches > 0 then
      wave.emit{a_signal = rec.full_name, b_signal = twin, mismatches = mismatches,
                first_cycle = first, a = av, b = bv}
    end
  end
end
wave.trailer("pairs", pairs)
wave.trailer("mismatches", total)
return total == 0
```

```sh
waveq -f $T eval '<the script above>'
# tb.dut.sum      tb.ref.sum      1 5 010 00f
# tb.dut.lane1.x  tb.ref.lane1.x  1 6 9   0
# pairs: 5   mismatches: 2
```

`pcall` skips signals without a twin (`tb.dut.only`, `tb.ref.extra`), which is
why the pair count is 5 — the same number `compare --pair-scopes tb.dut,tb.ref
--pair-by path --shift 0` reports, along with the same two divergent pairs.

### 6.7 Change-list analysis

`:changes()` is a single pass over a signal's value changes: use it for anything
about dwell times, glitches or activity, where per-cycle sampling would hide
same-cycle pairs.

```lua
local q1 = wave.signal("tb_top.q1")
wave.columns{"cycle", "tick", "value", "bits", "delta_cycles"}
local changes, anomalies, previous = q1:changes(), 0, nil
for _, ch in ipairs(changes) do
  local delta = nil
  if previous ~= nil and previous >= 0 and ch.cycle >= 0 then delta = ch.cycle - previous end
  if delta == 0 or (delta ~= nil and delta > 2) then
    anomalies = anomalies + 1
    wave.emit{cycle = ch.cycle, tick = ch.tick, value = ch.value, bits = ch.bits,
              delta_cycles = delta}
  end
  previous = ch.cycle
end
wave.trailer("changes", #changes)
wave.trailer("anomalies", anomalies)
return anomalies == 0
```

```
cycle tick   value bits      delta_cycles
32    325000 050   001010000 13
changes: 34
anomalies: 1
```

One anomaly: the next change after the stall starts is 13 cycles later — the
frozen window, visible as a delta. A `delta == 0` row would be a same-cycle
double change (a delta-cycle glitch) that the per-cycle view collapses into a
single value.

### 6.8 Auditing x/z

`:changes()` carries `has_x` per change and `bits` keeps the raw `x`/`z`
characters, so an audit is a filter:

```lua
local v = wave.signal("tb.v")
wave.columns{"cycle", "tick", "bits", "value", "int"}
local xz = 0
for _, ch in ipairs(v:changes()) do
  if ch.has_x then
    xz = xz + 1
    wave.emit{cycle = ch.cycle, tick = ch.tick, bits = ch.bits, value = ch.value,
              int = v:int(ch.cycle)}
  end
end
wave.trailer("xz_changes", xz)
return xz == 0
```

```sh
waveq -f $X eval '<the script above>'
# cycle tick bits value int
# 0     5    00xz x     -
```

`:int` is `-` (JSON `null`) for that sample, because decimal rendering of `x`/`z`
is meaningless; `v:is_xz(0)` is `true` and `v:value(0, "bin")` is `"00xz"`, so
both the raw bits and the boolean stay available. Nothing is silently coerced:
an x/z sample never compares equal to a numeric value unless you decide it
should.

### 6.9 Bit fields and signed arithmetic

`:int` gives you an `i64`, and Lua 5.4 has native bitwise operators, so decoding
a bus into fields is plain arithmetic:

```lua
local y = wave.signal("tb_top.ref_y")
wave.columns{"cycle", "value", "high5", "low4", "bit1", "sign"}
for _, c in ipairs{0, 13, 16} do
  local v = y:int(c)
  wave.emit{cycle = c, value = v, high5 = (v >> 4) & 0x1F, low4 = v & 0xF,
            bit1 = (v >> 1) & 1, sign = v < 0}
end
return true
```

```
cycle value high5 low4 bit1 sign
0     4     0     4    0    false
13    56    3     8    0    false
16    68    4     4    0    false
```

Signedness comes from the CLI/config, not from the script: the fixture's
`sn` is an 8-bit `reg signed` that the VCD does not mark as signed, so

```sh
waveq -f $F eval 'local s = wave.signal("tb_top.sn"); wave.emit{int = s:int(0), value = s:value(0), signed = s.signed}; return true'
# int: 253   value: fd   signed: false
waveq -f $F --signed-sig tb_top.sn eval 'local s = wave.signal("tb_top.sn"); wave.emit{int = s:int(0), value = s:value(0), signed = s.signed}; return true'
# int: -3    value: fd   signed: true
```

`--signed-sig`, `--signed`/`--unsigned`, `[types]` and `--radix` all arrive
through the handle's `signed`/`radix` fields, so one script reads the same
signedness the declarative commands would.

### 6.10 Joining golden data, and nested output

`wave.read_json` turns a JSON file into tables, which is enough to check a dump
against values produced by another tool (RTL simulation, a spreadsheet export, a
previous `--format json` run):

```lua
local y = wave.signal("tb_top.ref_y")
local golden = wave.read_json("tests/fixtures/data/golden_ref_y.json")
wave.columns{"cycle", "expected", "actual", "bits", "ok"}
local mismatches = 0
for _, row in ipairs(golden) do
  local actual = y:int(row.cycle)
  if actual ~= row.y then mismatches = mismatches + 1 end
  wave.emit{cycle = row.cycle, expected = row.y, actual = actual,
            bits = y:bits(row.cycle), ok = actual == row.y}
end
wave.trailer("rows_read", #golden)
wave.trailer("mismatches", mismatches)
return mismatches == 0
```

```
cycle expected actual bits      ok
13    56       56     000111000 true
10    44       44     000101100 true
rows_read: 2
mismatches: 0
```

Records may nest, which is how a script produces a self-describing JSON report:
`:at` already returns a table, so it can be emitted as-is.

```lua
local y = wave.signal("tb_top.ref_y")
wave.emit{cycle = 13, sample = y:at(13), changes_before = #y:changes(0, 12)}
return true
```

```json
{
  "changes_before": 13,
  "cycle": 13,
  "sample": {
    "bits": "000111000",
    "cycle": 13,
    "has_x": false,
    "tick": 135000,
    "time": "135ns",
    "value": "038"
  }
}
```

The key order *inside* a nested table follows Lua's table iteration order, so
only the values are meaningful (as with any JSON object); the top-level record's
keys are the fixed column list, which is why they come out sorted here. In text
mode the nested value renders as compact JSON (`{"bits":"…",…}`), which keeps a
wide report readable.

### 6.11 Reusable scripts

A script is a plain file; `[scripts]` gives it a name and a place in the config:

```toml
[scripts]
# a check that must stay green on every dump of this design
pipeline = { file = "scripts/expr_check.lua" }
# the stall-proof view, for dumps with a clock enable
transfers = { file = "scripts/transfer_scoreboard.lua" }
```

```sh
waveq -f dump.vcd run @pipeline          # exit 0/1 like a check
waveq -f dump.vcd run scripts/expr_check.lua --format json
```

Both produce the same document. Knobs (clock, side B, offsets, anchors, radix,
signedness) come from the command line and the config, so one script covers a
family of dumps; anything else is baked into the file, for example a table of
thresholds at the top:

```lua
local limits = {max_mismatch = 0, min_activity = 10}
```

There is no `argv` (see [§9](#9-limits-and-sharp-edges)) — when a threshold must
change per invocation, give each variant its own `[scripts]` entry, or read a
JSON settings file with `wave.read_json`.

### 6.12 Bundled example scripts

`scripts/` holds six small, self-contained examples, one idea each. Every one of
them exits 1 on the bundled fixture, because each reports a real finding there —
that is the point of the exit-code contract, not a broken example.

| script | dump | shows |
|---|---|---|
| `counter_increment.lua` | `counter.ghw` | invariant check, per-cycle sampling, exit 0 when clean |
| `xz_audit.lua` | `x_values.vcd` | walking `wave.signals()`, `has_x` per change, "no rows when clean" |
| `pair_scopes.lua` | `two_scopes.vcd` | name-driven pairing of `tb.dut.*` with `tb.ref.*`, `pcall` for missing twins |
| `expr_check.lua` | `pipeline.vcd` | an arithmetic predicate over earlier inputs (the flagship case) |
| `shift_scan.lua` | `pipeline.vcd` | a sweep emitted as a table plus a ranked summary |
| `transfer_scoreboard.lua` | `pipeline.vcd` | event-indexed comparison that survives a clock-enable stall |

```sh
waveq -f tests/fixtures/data/counter.ghw --no-config run scripts/counter_increment.lua
# signal: counter_tb.cnt
# cycles_checked: 20
# increments: 19
# anomalies: 0
# (no table: nothing to report)                            exit 0

waveq -f tests/fixtures/data/x_values.vcd --no-config run scripts/xz_audit.lua
# signal width xz_changes first_cycle bits
# tb.v   4     1          0           00xz
# signals_audited: 2  signals_with_xz: 1  xz_changes: 1     exit 1

waveq -f tests/fixtures/data/two_scopes.vcd --no-config run scripts/pair_scopes.lua
# a_signal       b_signal       mismatches first_cycle a   b
# tb.dut.sum     tb.ref.sum     1          5           010 00f
# tb.dut.lane1.x tb.ref.lane1.x 1          6           9   0
# pairs: 5  unpaired: 1  mismatches: 2                      exit 1

waveq -f tests/fixtures/data/pipeline.vcd --no-config run scripts/expr_check.lua
# cycle expected actual inputs
# 13    44       45     11+33
# checked: 14  mismatches: 1  first_divergent_cycle: 13      exit 1

waveq -f tests/fixtures/data/pipeline.vcd --no-config run scripts/shift_scan.lua
# shift compared mismatches first_cycle
# 0     20       20         0
# 3     17       1          13
# 8     12       12         8
# best_shift: 3  best_mismatches: 1                          exit 1

waveq -f tests/fixtures/data/pipeline.vcd --no-config run scripts/transfer_scoreboard.lua
# transfer a_cycle b_cycle a         b
# 10       10      13      000101100 000101101
# a_events: 34  b_events: 31  unpaired: 3  mismatches: 1      exit 1
```

Each file starts with a header comment naming its command and expected result,
and keeps the signal names, windows and thresholds as constants at the top, so
copying one and editing three lines is the intended way to reuse it. Every one
of them is executed by `tests/script.rs`, which is why the numbers above cannot
silently rot.

## 7. Performance

The cost model is small enough to keep in your head:

* **`wave.signal(name)` decodes that signal's whole change stream** (the same
  work `changes`/`table` do per `--signal`). Call it once per signal and keep the
  handle in a local: calling it inside a loop re-decodes the signal every time.
* **`:bits`/`:value`/`:int`/`:is_xz` are lookups** — a binary search over the
  time table plus one value fetch — so scanning every cycle of a few thousand
  cycles for a handful of signals is cheap.
* **`:changes()` is one pass** over the signal, and is the right tool for
  "every change" questions (dwell times, activity counts, glitch hunting). A
  per-cycle loop asks `n_cycles` questions to learn the same thing.
* **`:at`/`:at_tick` allocate a table per call**; in a hot loop prefer
  `:bits`/`:int` and build the record once, outside the loop, or emit nothing
  until you found something.
* **`wave.signals()` walks the hierarchy** and builds a record per variable;
  call it once, not per iteration.

Budgets: the default 50M instruction budget is far above what a sweep like §6.3
needs, and `--budget 0` removes the ceiling for long analyses. The budget hook
runs every `budget` instructions, capped at 1000 (so the default 50M budget
checks in 50 000 steps), which keeps the overhead negligible.

## 8. Errors, debugging, and scripts as checks

* `assert(cond, "message")` and `error("message")` surface as
  `waveq: runtime error: …` with a `stack traceback:` that names the chunk:
  `@path/to/script.lua:12` for a file, `eval:12` for `--format=eval` code.
  Syntax errors are reported the same way and also exit 2.
* `wave.note("scanning 4000 cycles")` is the progress channel (stderr, silenced
  by `--quiet`); `print` is for debugging under `--format text` and goes to
  stderr under `json`/`jsonl` so the document stays parseable.
* `pcall` makes a loop tolerant: `pcall(wave.signal, name)` returns
  `false, "runtime error: no signal …"` for a missing name (see §6.6) instead of
  aborting the run.
* The exit code is the contract: `return mismatches == 0` turns a script into a
  check with the same 0/1/2 semantics as `compare`, and `wave.trailer` carries
  the numbers a shell or a test wants to read without parsing rows.
* Because the schema is fixed by the first `wave.emit` (or `wave.columns`),
  `--format json` output is stable enough to diff in a regression test — that is
  how `tests/script.rs` pins the API.

## 9. Limits and sharp edges

* **No `argv`.** A script cannot read command-line arguments, environment
  variables or files other than those passed to `wave.read_json`. Bars, anchors
  and offsets reach it through flags; everything else is baked in.
* **No `io`, `os`, `debug`, `package`, `require`, `loadfile`, `dofile`.** A script
  cannot write a file: emit rows (or `print` under `--format text`) and let the
  shell redirect. `load` (Lua 5.4's string compiler; `loadstring` no longer
  exists) is available.
* **One chunk per invocation**, no modules and no `require`: keep helpers as
  locals in the file, or inline them.
* **The CLI's cycle windows do not apply.** `--from-cycle`/`--to-cycle` belong to
  `table`/`changes`/`compare`; a script loops over the range it wants (and should
  say so in the trailer).
* **`--max-rows` does not truncate script rows.** Trim in the script.
* **Records must be JSON-representable**: tables, numbers, strings, booleans and
  `nil`. Handles, functions and other userdata in a record fail the emit
  (`unsupported value type`) — pass `:bits`/`:value`/`:int` instead.
* **Column list is fixed by the first emit**: emit a stable schema (or declare it
  with `wave.columns`) rather than growing keys as you discover them.
* **`--clock none`** removes every cycle-indexed call; only `wave.tick`,
  `wave.time` and `:at_tick` remain meaningful.
* **Cycle space is only valid before the first stall** in a design with clock
  enable. Prefer transfer-indexed windows for anything that must hold across a
  stall.
* **`--memory` bounds the Lua state, not the dump**: allocating huge Lua tables
  (`for i = 1, 1e9 do t[i] = i end`) trips the limit; decoding signals does not.
* **`--budget`/`--memory` are subcommand options**, so they come after `run`/`eval`.
* **`--expect-file` is not used** by scripts: `compare`'s `@file` B-sides are
  CSV/JSONL, while `wave.read_json` reads a JSON document. Keep expected values
  as JSON for a script.

## 10. See also

* `waveq eval --help` — the API in the CLI's own words (also shown by `run --help`).
* `scripts/` — the six example scripts listed in §6.12, each with its command and
  expected result in a header comment.
* `SKILL.md` — the command-oriented workflow guide and the `[scripts]` section of
  the config file.
* `tests/script.rs` — end-to-end tests for every behaviour described here (and
  for the example scripts), and the place to look when the document and
  implementation ever disagree.
* `tests/fixtures/src/pipeline.sv` — the fixture used throughout: a golden
  reference, a 3-deep pipeline, a clock-enable stall and one injected bug.
