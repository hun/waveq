// waveq fixture: golden reference model (unpipelined) vs 3-stage pipelined DUT
// in a single dump, with a global clock-enable stall and one injected
// single-cycle bug.
//
// The stall is a real clock enable: inputs, data stages and the valid chain all
// hold while it is asserted. Cycle-space comparison therefore only works inside
// the pre-stall window (a fixed shift is broken while the pipeline refills),
// while the transfer-indexed comparison stays exact and reports the trailing
// unpaired accepted inputs (== pipeline depth).
//
// Load-bearing properties (see tests/cli.rs for the exact expected numbers):
//   * cycle c is the posedge at tick 5 + 10*c ns; sampling is post-edge
//   * a_c = c+1, b_c = 3(c+1), ref_y_c = 4(c+1)             before the stall
//   * q1_c = ref_y[c-1], q2_c = ref_y[c-2], q3_c = ref_y[c-3] before the stall
//   * the q2 load at cycle 12 is corrupted once -> q3 diverges at cycle 13 only
//   * ov_c == in_valid[c-3] in progress cycles, and out_go = ov & ~stall marks
//     output transfers, so the k-th out_go-high cycle carries the k-th accepted
//     input's value (transfer alignment = shift 0)
`timescale 1ns/1ps
module ref_model(input [7:0] a, b, output [8:0] y);
  assign y = a + b;
endmodule

module tb_top;
  reg clk = 0;
  reg rst = 1;
  reg [7:0] a = 0, b = 0;
  reg stall = 0;
  reg [7:0] cyc = 0;
  reg [8:0] q1 = 0, q2 = 0, q3 = 0;   // pipeline data, 3 sampled stages
  reg v1 = 0, v2 = 0, ov = 0;         // output valid: same latency as q3
  reg signed [7:0] sn = -3;           // signedness/radix tests
  wire [8:0] ref_y, dut_y;
  wire in_valid = ~stall;
  wire out_go = ov & in_valid;        // output transfer: a held output is not a new transfer

  ref_model u_ref(.a(a), .b(b), .y(ref_y));
  assign dut_y = q3;

  always #5 clk = ~clk;               // posedges at 5,15,25,... ns -> cycle c at tick 5+10c ns

  always @(posedge clk) begin
    cyc <= cyc + 1;
    // clock enable: inputs advance only while the design progresses, so the
    // reference sees exactly the accepted input stream
    if (!stall) begin
      a <= a + 1;
      b <= b + 3;
    end
    if (rst) begin
      rst <= 1'b0;                    // one cycle of reset so the valid chain starts clean
      q1 <= 0; q2 <= 0; q3 <= 0;
      v1 <= 0; v2 <= 0; ov <= 0;
    end else if (!stall) begin
      q1 <= a + b;
      q2 <= (cyc == 12) ? q1 + 1 : q1;  // injected bug, one cycle only
      q3 <= q2;
      v1 <= in_valid; v2 <= v1; ov <= v2;  // ov_c == in_valid[c-3], i.e. exactly q3's latency
    end
  end

  initial begin
    $dumpfile("data/pipeline.vcd");     // relative to the CWD that runs vvp
    $dumpvars(0, tb_top);
    #200 stall = 1;                     // stalls cycles 20..31 (12 cycles)
    #120 stall = 0;
    #140 $finish;                       // t=460ns, last posedge 455ns -> cycles 0..45
  end
endmodule
