-- pair_scopes.lua -- compare every signal with its same-leaf twin in another scope.
--
--   waveq -f tests/fixtures/data/two_scopes.vcd --no-config run scripts/pair_scopes.lua
--   # expected: two rows (tb.dut.sum, tb.dut.lane1.x), `pairs: 5`, `mismatches: 2`, exit 1
--
-- The scripted form of `compare --pair-scopes tb.dut,tb.ref --pair-by path
-- --shift 0`: same pairing rule, but the script keeps the detail a reviewer
-- wants (first divergent cycle and the two values there), and it counts the
-- signals that have no twin instead of only warning about them.

local glob_a, glob_b = "tb.dut.*", "tb.ref.*"
local prefix_a, prefix_b = "^tb%.dut%.", "tb.ref."

wave.columns{"a_signal", "b_signal", "mismatches", "first_cycle", "a", "b"}
local pairs, unpaired, divergent, mismatches_total = 0, 0, 0, 0
for _, rec in ipairs(wave.signals(glob_a)) do
  local twin = (rec.full_name:gsub(prefix_a, prefix_b))
  local ok_a, a = pcall(wave.signal, rec.full_name)
  local ok_b, b = pcall(wave.signal, twin)
  if not (ok_a and ok_b) then
    unpaired = unpaired + 1
  else
    pairs = pairs + 1
    local mismatches, first_cycle, a_value, b_value = 0, nil, nil, nil
    for c = 0, wave.last_cycle() do
      if a:bits(c) ~= b:bits(c) then
        mismatches = mismatches + 1
        if first_cycle == nil then
          first_cycle, a_value, b_value = c, a:value(c), b:value(c)
        end
      end
    end
    mismatches_total = mismatches_total + mismatches
    if mismatches > 0 then
      divergent = divergent + 1
      wave.emit{a_signal = rec.full_name, b_signal = twin, mismatches = mismatches,
                first_cycle = first_cycle, a = a_value, b = b_value}
    end
  end
end

wave.trailer("pairs", pairs)
wave.trailer("unpaired", unpaired)
wave.trailer("divergent_pairs", divergent)
wave.trailer("mismatches", mismatches_total)
return mismatches_total == 0
