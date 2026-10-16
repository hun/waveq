-- xz_audit.lua -- list every signal that ever carries x or z.
--
--   waveq -f tests/fixtures/data/x_values.vcd --no-config run scripts/xz_audit.lua
--   # expected: one row (tb.v, first_cycle 0, bits 00xz), `signals_audited: 2`, exit 1
--
-- A clean dump emits no rows at all and exits 0, so this is a "no unknowns in
-- this block" gate. The audit walks the hierarchy (wave.signals) and then one
-- change list per signal, which is the cheap direction: x/z can only appear in
-- a change, never between two neighbouring ones.

local pattern = nil -- e.g. "tb.q*" to audit a subset

wave.columns{"signal", "width", "xz_changes", "first_cycle", "bits"}
local audited, offenders, xz_total = 0, 0, 0
for _, rec in ipairs(wave.signals(pattern)) do
  local signal = wave.signal(rec.full_name)
  audited = audited + 1
  local xz, first_cycle, bits = 0, nil, nil
  for _, change in ipairs(signal:changes()) do
    if change.has_x then
      xz = xz + 1
      if first_cycle == nil then first_cycle, bits = change.cycle, change.bits end
    end
  end
  if xz > 0 then
    offenders = offenders + 1
    xz_total = xz_total + xz
    wave.emit{signal = rec.full_name, width = rec.width, xz_changes = xz,
              first_cycle = first_cycle, bits = bits}
  end
end

wave.trailer("signals_audited", audited)
wave.trailer("signals_with_xz", offenders)
wave.trailer("xz_changes", xz_total)
return offenders == 0
