#!/usr/bin/env bash
# Regenerate the tool-generated fixtures in tests/fixtures/data/.
# Run manually; the outputs are committed so `cargo test` needs no EDA tools.
#
# tests/fixtures/data/x_values.vcd (x/z coverage) and
# tests/fixtures/data/two_scopes.vcd (dut./ref. scope pairing, nested lanes,
# injected +1) are hand-written and are NOT touched by this script.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p data

# --- SystemVerilog: golden reference vs 3-stage pipeline, CE stall, injected bug
iverilog -g2012 -o /tmp/waveq_pipeline.vvp src/pipeline.sv
vvp /tmp/waveq_pipeline.vvp                  # $dumpfile path is relative to this CWD
vcd2fst data/pipeline.vcd data/pipeline.fst

# --- VHDL/GHW: unsigned counter + constant signed value (fs timescale, GHW)
rm -rf /tmp/waveq_ghdl && mkdir -p /tmp/waveq_ghdl
ghdl -a --std=08 --workdir=/tmp/waveq_ghdl src/counter_tb.vhd
ghdl -e --std=08 --workdir=/tmp/waveq_ghdl -o /tmp/waveq_ghdl/counter_tb counter_tb
/tmp/waveq_ghdl/counter_tb --wave=data/counter.ghw --stop-time=200ns

ls -l data
