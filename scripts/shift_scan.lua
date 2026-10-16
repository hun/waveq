-- shift_scan.lua -- rank every shift between two signals by mismatch count.
--
--   waveq -f tests/fixtures/data/pipeline.vcd --no-config run scripts/shift_scan.lua
--   # expected: nine rows, `best_shift: 3`, `best_mismatches: 1`, exit 1
--
-- `align` answers exactly this for one shape of sweep; the scripted version
-- exists so the table can be reordered, filtered or joined with more columns
-- (input values, expected values, the transfer index) before it is printed.
-- The per-shift counts here are the ones `align --a tb_top.ref_y --b tb_top.q3
-- --shifts 0..=8 --from-cycle 0 --to-cycle 19` reports.

local reference, dut = "tb_top.ref_y", "tb_top.q3"
local max_shift, from_cycle, to_cycle = 8, 0, 19

local a, b = wave.signal(reference), wave.signal(dut)

wave.columns{"shift", "compared", "mismatches", "first_cycle"}
local best
for s = 0, max_shift do
  local compared, mismatches, first_cycle = 0, 0, nil
  for c = from_cycle + s, to_cycle do
    compared = compared + 1
    if a:bits(c - s) ~= b:bits(c) then
      mismatches = mismatches + 1
      if first_cycle == nil then first_cycle = c end
    end
  end
  wave.emit{shift = s, compared = compared, mismatches = mismatches,
            first_cycle = first_cycle}
  if best == nil or mismatches < best.mismatches then
    best = {shift = s, mismatches = mismatches, first_cycle = first_cycle}
  end
end

wave.trailer("a", a.name)
wave.trailer("b", b.name)
wave.trailer("best_shift", best.shift)
wave.trailer("best_mismatches", best.mismatches)
wave.trailer("best_first_cycle", best.first_cycle)
return best.mismatches == 0
