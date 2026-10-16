-- expr_check.lua -- does the DUT output equal an expression over earlier inputs?
--
--   waveq -f tests/fixtures/data/pipeline.vcd --no-config run scripts/expr_check.lua
--   # expected: one row (cycle 13, expected 44, actual 45), `mismatches: 1`, exit 1
--
-- The canonical script-mode question: no flag can spell "q3[c] == a[c-3] +
-- b[c-3]", because the condition is arithmetic over a previous cycle. The
-- window stops at cycle 16 on purpose: the fixture's clock-enable stall
-- (cycles 20..31) freezes the inputs, so the expression only means something
-- while the pipeline is being fed (see transfer_scoreboard.lua for the
-- stall-proof view).

local output = "tb_top.q3"
local inputs = {"tb_top.a", "tb_top.b"}
local latency = 3
local from_cycle, to_cycle = latency, 16

local out = wave.signal(output)
local ins = {}
for _, name in ipairs(inputs) do
  ins[#ins + 1] = wave.signal(name)
end

local function expected_at(cycle)
  local sum = 0
  for _, signal in ipairs(ins) do
    sum = sum + signal:int(cycle - latency)
  end
  return sum
end

wave.columns{"cycle", "expected", "actual", "inputs"}
local checked, mismatches, first_cycle = 0, 0, nil
for c = from_cycle, to_cycle do
  local expected, actual = expected_at(c), out:int(c)
  checked = checked + 1
  if actual ~= expected then
    mismatches = mismatches + 1
    if first_cycle == nil then first_cycle = c end
    local values = {}
    for _, signal in ipairs(ins) do
      values[#values + 1] = tostring(signal:int(c - latency))
    end
    wave.emit{cycle = c, expected = expected, actual = actual,
              inputs = table.concat(values, "+")}
  end
end

wave.trailer("output", out.name)
wave.trailer("latency", latency)
wave.trailer("checked", checked)
wave.trailer("mismatches", mismatches)
wave.trailer("first_divergent_cycle", first_cycle)
return mismatches == 0
