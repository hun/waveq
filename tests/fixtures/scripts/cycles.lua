-- waveq fixture: the smallest [scripts] entry that still proves the API works.
-- ref_y[13] = 0x038 = 56 (see tests/fixtures/src/pipeline.sv).
local y = wave.signal("tb_top.ref_y")
wave.emit{cycle = 13, value = y:value(13), int = y:int(13)}
return y:int(13) == 56
