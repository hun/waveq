-- transfer_scoreboard.lua -- pair accepted inputs with output events, ignoring cycles.
--
--   waveq -f tests/fixtures/data/pipeline.vcd --no-config run scripts/transfer_scoreboard.lua
--   # expected: one row (transfer 10, a_cycle 10, b_cycle 13), `unpaired: 3`, exit 1
--
-- A clock-enable stall makes cycle-space comparison lie: cycles pass without a
-- transfer, so a fixed shift stops lining the stages up while the pipeline
-- refills, and the tail of any cycle window diverges by construction. Counting
-- events instead of cycles stays exact. This is `compare --align transfers
-- --in-valid tb_top.in_valid --out-valid tb_top.out_go --map
-- tb_top.ref_y=tb_top.q3`, with the unpaired events reported explicitly.

local in_valid, out_valid = "tb_top.in_valid", "tb_top.out_go"
local signal_a, signal_b = "tb_top.ref_y", "tb_top.q3"

local accept, output = wave.signal(in_valid), wave.signal(out_valid)
local a, b = wave.signal(signal_a), wave.signal(signal_b)

-- 1. Collect the two event streams in cycle order.
local events_a, events_b = {}, {}
for c = 0, wave.last_cycle() do
  if accept:bits(c) == "1" then
    events_a[#events_a + 1] = {cycle = c, bits = a:bits(c)}
  end
  if output:bits(c) == "1" then
    events_b[#events_b + 1] = {cycle = c, bits = b:bits(c)}
  end
end

-- 2. Compare the k-th event of each stream: latency and stalls disappear.
wave.columns{"transfer", "a_cycle", "b_cycle", "a", "b"}
local mismatches = 0
for k = 1, math.min(#events_a, #events_b) do
  if events_a[k].bits ~= events_b[k].bits then
    mismatches = mismatches + 1
    wave.emit{transfer = k - 1, a_cycle = events_a[k].cycle, b_cycle = events_b[k].cycle,
              a = events_a[k].bits, b = events_b[k].bits}
  end
end

wave.trailer("a_events", #events_a)
wave.trailer("b_events", #events_b)
-- Accepted inputs whose output event has not happened yet: the pipeline depth,
-- not a failure.
wave.trailer("unpaired", math.abs(#events_a - #events_b))
wave.trailer("mismatches", mismatches)
return mismatches == 0
