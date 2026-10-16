-- counter_increment.lua -- a counter must advance by exactly one per cycle.
--
--   waveq -f tests/fixtures/data/counter.ghw --no-config run scripts/counter_increment.lua
--   # expected: no rows at all, `increments: 19`, `anomalies: 0`, exit 0
--
-- The simplest useful script: sample one signal per cycle and check an
-- invariant. If the counter stalls, jumps or wraps, one row per anomaly is
-- emitted and the exit code is 1, so it works as a check in a shell pipeline.
-- Edit the three names at the top for another simple counter dump.

local counter = "counter_tb.cnt"
local from_cycle = 0
local to_cycle = nil -- nil = the resolved clock's last cycle

local cnt = wave.signal(counter)
local last = to_cycle or wave.last_cycle()

wave.columns{"cycle", "previous", "value", "delta"}
local anomalies, previous, steps = 0, nil, 0
for c = from_cycle, last do
  local value = cnt:int(c)
  if value == nil then
    anomalies = anomalies + 1
    wave.emit{cycle = c, previous = previous, delta = nil}
  elseif previous ~= nil then
    steps = steps + 1
    local delta = value - previous
    if delta ~= 1 then
      anomalies = anomalies + 1
      wave.emit{cycle = c, previous = previous, value = value, delta = delta}
    end
  end
  previous = value
end

wave.trailer("signal", cnt.name)
wave.trailer("cycles_checked", last - from_cycle + 1)
wave.trailer("increments", steps)
wave.trailer("anomalies", anomalies)
return anomalies == 0
