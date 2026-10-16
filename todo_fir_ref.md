# todo — what `waveq` was missing for the `fir_ref` composition debugging case

Context. The case is `verilog_templates/modules/fir_ref`: a **reference composition** of
eight RTL blocks (schedule, delay line, coefficient RAM, DSP lanes, accumulator, sum tree,
tail) whose golden model in C++ is a *structural composition of the submodule models*. The
failure mode was "the model matches the DUT on constant data and diverges on impulses", so
the question was "which of the ~20 internal signals is the first to go wrong", not "is the
pipeline latency 3". The dumps were produced with Verilator (DUT, via a wrapper that
exposes internals as ports) and [vcd-writer](https://github.com/favorart/vcd-writer) (the
C++ model, registered from inside the same testbench), merged into **one** VCD.

## What worked, and is the whole reason this got solved

Worth stating because it is the pattern the tool is for, and a refactor must not break it:

1. One dump, two scopes (`dut.*` and `ref.*`), same signal names both sides.
2. **One `compare` with every stage pair at `--shift 0`, read the `pairs:` table's
   `first_divergent_cycle` column.** That single command turned "3409 mismatches" into
   "`chain0` at cycle 338, everything else later". Iterating that (adding per-lane products,
   then per-lane *inputs*) walked the bug back stage by stage: lane 1–3 products diverged
   while all their inputs matched → the PCIN chain link. Then the schedule diverged →
   `iv_r`/`os_r`. Both were one-line model bugs, found in minutes after hours of reasoning
   had failed.
3. `table --cycles A..B --signal <the two sides + the enable>` to read the mechanism
   (`ce=0` in the row before the divergence, `tready` in the row after, …).
4. `align --a X --b Y --shifts 0..=0` to establish the base latency before trusting any
   divergence (the composition is bit-exact at shift 0, so every divergence is real).
5. Dumping the **driven inputs** (`ce`, `rst`, `tvalid`, `tready`) into the same VCD was
   what made the `iv_r` divergence interpretable. Cheap and worth recommending in the skill
   for the "first divergent internal signal" workflow: a waveform with only internal signals
   cannot say "this happened on the first stall".

## Missing / awkward, in priority order

### 1. Compare side A in one file against side B in another (`--b-file`)

The single biggest gap. Comparing a golden model that lives in *another* process,
simulator or language against a Verilator dump is the normal case (VHDL vs Verilog, C++
model vs RTL, two simulator runs). Today the reference must be a signal in the *same* file,
so I had to inject the C++ model's samples into the DUT's VCD (vcd-writer in the testbench)
instead of dumping both sides independently and letting waveq join them. If the two sides
come from different tools with different clock phases, this is not even a workaround.

Sketch:
```sh
waveq -f dut.vcd --b-file ref.vcd \
  compare --map dut.u_a.a0=ref.a0 --shift 0        # A from -f, B from --b-file
waveq -f dut.vcd --b-file ref.vcd align --a dut.x --b ref.y --shifts 0..=8
```
Clock resolution would need one clock per side (`--clock` for A, `--b-clock` for B), and
`--b-cycle-offset N` for a known constant skew between the two runs.

### 2. Pair many signals without hand-writing every `--map` (`--map-glob`, `--pair-by`)

A 20-signal pair list is 20 `--map A=B:0` arguments. Two failure modes: a typo silently
compares nothing, and the *pairs table* is the primary output, so a missing pair is a
missing answer. I ended up generating the map with a shell loop, which is exactly the kind
of glue the tool exists to remove.

Sketch (pair by leaf name across two scopes):
```sh
waveq -f d.vcd compare --pair-by leaf --pair-scopes dut,ref --shift 0
waveq -f d.vcd compare --map-glob 'dut.*=ref.*' --shift 0     # same leaf names
```
and a config form:
```toml
[compare.pairs]
"dut.*" = "ref.*"      # leaf-name pairing
```

### 3. A numeric `--delta` column and a delta summary

For arithmetic stages, "mismatch" is the wrong unit: what I needed to know was *how* wrong
(off by one sample? scaled? one bit?) to choose the next probe. The pairs table shows
counts and the first divergent cycle, and rows show `a`/`b`, but there is no
`delta = b - a` column or a `max_abs_delta` per pair. With signed values and `--cmp
numeric` a delta is well defined; a `--delta` column plus a `max_delta` summary field would
have compressed several iterations into one.

Also useful in the same vein: a `bits_differ` mask column (`a xor b`) — for a bit-exact
comparison of a 48-bit accumulator, "which bits" localises a wrap vs a scale error
immediately.

### 4. Compare against expected values that are not a signal (`--expect-file`)

The mirror of gap 1: sometimes the reference is a *file of expected values per cycle*
(CSV/JSONL from a Python model, a scoreboard log, a golden vector file), not a VCD signal
and not something you can inject. Something like:
```sh
waveq -f dut.vcd compare --map dut.dout=@expect.csv:cycle,value --shift 0 --delta
```
would cover the "golden model logged its outputs" case, and pairs naturally with the
repo's own convention of reporting a *metric against a documented bound* rather than a bare
boolean (`--max-delta 0` vs `--max-delta 1`).

### 5. A cycle-window cut / export (`cut` or `compare --emit`)

A failing dump is 4 MB / 4036 cycles; the interesting part is 40 cycles. I wanted to hand
someone (and eventually commit) a *small* VCD with just the divergence. Producer-side
window dumping is a per-simulator chore; a reader-side cut is generic:
```sh
waveq -f big.vcd cut --cycles 2035..2075 --signals 'dut.*,ref.*,tb.ce,tb.rst' -o small.vcd
```
Bonus: the same command with the *pairs* of a failing check resolved would make a
self-contained regression fixture out of any failure.

### 6. Cross-dump / cross-convention cycle alignment

Even with gaps 1–2 solved, two sides from different tools rarely agree on cycle 0 (reset
length, flush, initial `x`). `align` finds a shift for one pair, but with several pairs it
needs the *same* shift for all of them; when the two runs have different reset lengths the
right primitive is an event anchor ("align both sides on the first cycle where
`<signal>` pulses"). `--align transfers` does this for in/out streams; a
`--anchor NAME` (cycle 0 = the first edge where NAME is 1) would extend it to the internal,
non-stream case that a stalled datapath actually has.

### 7. Small ergonomics

* `signals` is a flat list; with `dut.*`/`ref.*` scopes a `--tree` or a "same leaf, two
  scopes" view would make pairing obvious (and would make gap 2 cheap to implement).
* Cycles mode requires a shift on every pair; when a user says `--map A=B` without one,
  the error is good, but suggesting `--shift 0`/`--auto-shift` in it saves a round trip.
* `--probe` samples signals at the A-side cycle (the driver of the expected value), which
  is right, but a `--probe-at b_cycle` variant (sample at the *divergent* cycle) is what I
  reached for when reading a mechanism (`ce`/`tready` *at* the divergence). I worked around
  it with `table --cycles`.
* Documentation of the *composition* case (many pairs, one dump, walk back the earliest
  divergent stage) would sit nicely next to the existing golden-vs-pipelined workflow in
  SKILL.md — it is the same idea with `first_divergent_cycle` as the answer rather than
  `best_shift`.

## Recipe that made this work (candidate for SKILL.md)

Dumping a C++ golden model next to a Verilator DUT into one file, which is what removes the
need for gap 1 in the single-process case:

```cpp
// once: register both sides, same leaf names, different scopes
HeadPtr head = makeVCDHeader(TimeScale::ONE, TimeScaleUnit::ns, "2026-01-01");
VCDWriter w("dump.vcd", head);
auto clk = w.register_var("tb",  "clk",   VariableType::wire, 1);
auto a0d = w.register_var("dut", "a0",    VariableType::wire, 18);
auto a0r = w.register_var("ref", "a0",    VariableType::wire, 18);
// per cycle, post-edge (waveq samples post-edge), 1 pump of the clock per cycle:
w.change(clk, 10*c,     "0");  w.change(clk, 10*c + 5, "1");
w.change(a0d, 10*c + 5, bits(dut_signal, 18));
w.change(a0r, 10*c + 5, bits(model_value, 18));
```
* the dumped value must be the **post-edge** state on both sides, otherwise every pair looks
  shifted by one cycle;
* signed signals: VCD carries no signedness, so either `--signed` or `[types]` in the
  config;
* wide values: emit a bit string of the *port's* width (MSB first), two's complement for
  negatives — then `--cmp bit-exact` (the default) is meaningful and `--cmp numeric` is
  available for width-lenient comparison.
