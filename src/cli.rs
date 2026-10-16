//! Command line surface (clap derive).

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::format::{AlignMode, CmpMode, Edge, Radix, XMode};

/// Where `compare --probe` samples its signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProbeAt {
    /// The cycle that produced the expected value (shift-shifted A cycle)
    ACycle,
    /// The divergent cycle as observed on side B
    BCycle,
}
use crate::output::Format;

#[derive(Parser, Debug)]
#[command(
    name = "waveq",
    version,
    about = "Query VCD/FST/GHW waveform dumps (clock cycles, values, change lists, pipeline alignment)",
    after_help = "Cycle numbers come from the resolved clock's rising edges with cycle 0 = first edge and \
                  post-edge sampling. See SKILL.md for the golden-vs-pipelined workflow.",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone, Default)]
pub struct Global {
    /// Wave file (VCD, FST or GHW) — side A
    #[arg(short = 'f', long, global = true, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// Second wave file holding side B (default: side B is in -f)
    #[arg(long, global = true, value_name = "PATH")]
    pub b_file: Option<PathBuf>,

    /// Default expected-value file for `--map A=@:cycle,value` B sides
    #[arg(long, global = true, value_name = "PATH")]
    pub expect_file: Option<PathBuf>,

    /// Clock signal for side A (`none` = tick/time only)
    #[arg(long, global = true, value_name = "NAME|none")]
    pub b_clock: Option<String>,

    /// Skip the first N cycles of side B (align two runs with different preamble)
    #[arg(long, global = true, value_name = "N", allow_hyphen_values = true)]
    pub b_cycle_offset: Option<i64>,

    /// Skip the first N cycles of side A (e.g. reading a `cut` window with the
    /// source's cycle numbers: `--cycle-offset 2040`)
    #[arg(long, global = true, value_name = "N", allow_hyphen_values = true)]
    pub cycle_offset: Option<i64>,

    /// Logical cycle 0 of side A is the first cycle where NAME is 1
    #[arg(long, global = true, value_name = "NAME")]
    pub anchor: Option<String>,

    /// Logical cycle 0 of side B is the first B cycle where NAME is 1
    #[arg(long, global = true, value_name = "NAME")]
    pub b_anchor: Option<String>,

    /// Explicit config file (default: first .waveq.toml from CWD upward)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Disable config discovery
    #[arg(long, global = true)]
    pub no_config: bool,

    /// Clock signal (`none` = tick/time only)
    #[arg(long, global = true, value_name = "NAME|none")]
    pub clock: Option<String>,

    /// Clock edge used for cycle numbering
    #[arg(long, global = true, value_enum, value_name = "posedge|negedge|both")]
    pub edge: Option<Edge>,

    /// Output radix for bit-vectors
    #[arg(long, global = true, value_enum, value_name = "auto|bin|hex|oct|dec")]
    pub radix: Option<Radix>,

    /// Treat all signals as signed
    #[arg(long, global = true, conflicts_with = "unsigned")]
    pub signed: bool,

    /// Treat all signals as unsigned (built-in default)
    #[arg(long, global = true)]
    pub unsigned: bool,

    /// Mark NAME as signed (repeatable)
    #[arg(long = "signed-sig", global = true, value_name = "NAME")]
    pub signed_sig: Vec<String>,

    /// Mark NAME as unsigned (repeatable)
    #[arg(long = "unsigned-sig", global = true, value_name = "NAME")]
    pub unsigned_sig: Vec<String>,

    /// Output format
    #[arg(long, global = true, value_enum, value_name = "text|json|jsonl")]
    pub format: Option<Format>,

    /// Maximum rows to print (0 = unlimited)
    #[arg(long, global = true, value_name = "N")]
    pub max_rows: Option<usize>,

    /// Suppress stderr notes (never errors)
    #[arg(long, global = true)]
    pub quiet: bool,
}

/// Options shared by `run` and `eval`.
#[derive(Args, Debug, Clone, Default)]
pub struct ScriptArgs {
    /// Instruction budget (0 = unlimited)
    #[arg(long, value_name = "N", default_value_t = 50_000_000)]
    pub budget: u64,

    /// Lua memory limit in MiB (0 = unlimited)
    #[arg(long, value_name = "MIB", default_value_t = 256)]
    pub memory: u64,
}

/// Long help for `run`/`eval`: the frozen script API.
const SCRIPT_API_HELP: &str = "\
Runs a Lua 5.4 script against the loaded dump (side A, plus side B when the
--b-* options are used). The script gets one global, `wave`:

  wave.sides()                     -- {'a'} or {'a','b'}
  wave.path([side])                -- dump path of that side
  wave.info()                      -- file, format, version, date, timescale,
                                      scopes, vars, tick_count, first_tick,
                                      first_time, last_tick, last_time
  wave.clock([side])               -- clock table (clock --format json fields)
  wave.cycles([side])              -- number of clock edges
  wave.last_cycle([side])          -- highest logical cycle
  wave.tick(cycle, [side])         -- tick of a logical cycle (nil out of range)
  wave.time(tick)                  -- tick as time text (135ns)

  wave.signals([pattern], [side])  -- signal records (pattern is a glob)
  wave.signal(name, [side])        -- handle; NAME or NAME[msb:lsb]
    fields: name spec width signed radix kind direction index
    methods: :at(c) :at_tick(t) :bits(c) :value(c [,radix]) :int(c)
             :is_xz(c) :changes([from_cycle, [to_cycle]])

  wave.emit(record)                -- one output row
  wave.columns{...}                -- column order, before the first emit
  wave.trailer(key, value)         -- extra field after the table
  wave.note(msg)                   -- stderr note (suppressed by --quiet)
  wave.encode_json(v)  wave.decode_json(s)  wave.read_json(path)

Cycle arguments are logical cycles (cycle 0 = the side's first edge, unless
--cycle-offset/--anchor moved it). The exit code is 0 when the script returns
nil or true, 1 when it returns false, and 2 for a script error or an exceeded
--budget/--memory limit. `print` writes to stdout for --format text and to
stderr otherwise, so json/jsonl output stays parseable.";

#[derive(Subcommand, Debug)]
pub enum Command {
    /// File format, version, timescale, scope/signal counts
    Info {
        /// Also read the body (tick range)
        #[arg(long)]
        full: bool,
        /// Also report the resolved clock (named --with-clock so that the
        /// global `--clock <NAME>` keeps working after the subcommand)
        #[arg(long = "with-clock")]
        with_clock: bool,
    },

    /// List scopes
    Scopes {
        /// Only scopes whose full name matches GLOB (repeatable)
        #[arg(long = "match", value_delimiter = ',', value_name = "GLOB")]
        match_: Vec<String>,
        /// Maximum scope depth to print (0 = top-level scopes)
        #[arg(long, value_name = "N")]
        depth: Option<usize>,
    },

    /// List signals with width, type, direction and signedness
    Signals {
        /// Only signals whose full name matches GLOB (repeatable)
        #[arg(long = "match", value_delimiter = ',', value_name = "GLOB")]
        match_: Vec<String>,
        /// Only signals under this scope prefix
        #[arg(long, value_name = "PREFIX")]
        scope: Option<String>,
        /// Print only the number of matching signals
        #[arg(long)]
        count: bool,
        /// Print the scope tree instead of a flat list
        #[arg(long)]
        tree: bool,
    },

    /// Details about one signal
    Signal {
        /// Signal name (NAME or NAME[msb:lsb])
        name: String,
        /// Bit range to inspect, e.g. 15:8
        #[arg(long, value_name = "MSB:LSB")]
        bits: Option<String>,
    },

    /// Resolved clock: edges, period, candidates
    Clock {
        /// Print only the candidate table
        #[arg(long)]
        list: bool,
    },

    /// Effective configuration (after discovery/merge)
    Config,

    /// Signal value at a time or cycle
    Value {
        /// Sample time (10ns) or tick
        #[arg(long, value_name = "TIME")]
        at: Option<String>,
        /// Sample at clock cycle N
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        cycle: Option<i64>,
        /// Signal to sample (repeatable, comma separated, @group allowed)
        #[arg(long, value_delimiter = ',', value_name = "NAME")]
        signal: Vec<String>,
    },

    /// Value-change list per signal
    Changes {
        /// Signals to inspect (repeatable, comma separated, @group allowed)
        #[arg(long, value_delimiter = ',', value_name = "NAME")]
        signal: Vec<String>,
        /// Only changes at or after this cycle
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        from_cycle: Option<i64>,
        /// Only changes at or before this cycle
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        to_cycle: Option<i64>,
        /// Only changes at or after this time/tick
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// Only changes at or before this time/tick
        #[arg(long, value_name = "TIME")]
        to: Option<String>,
        /// Add delta_cycles against the previous change of the same signal
        #[arg(long)]
        delta: bool,
        /// Drop the initial value of each signal
        #[arg(long)]
        no_initial: bool,
        /// Keep only changes that happen exactly at a clock edge
        #[arg(long)]
        at_edges_only: bool,
    },

    /// Per-cycle table of sampled signal values
    Table {
        /// Signals to tabulate (repeatable, comma separated, @group allowed)
        #[arg(long, value_delimiter = ',', value_name = "NAME")]
        signal: Vec<String>,
        /// Cycle range A..B
        #[arg(long, value_name = "A..B")]
        cycles: Option<String>,
        /// First cycle of the range
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        from_cycle: Option<i64>,
        /// Last cycle of the range
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        to_cycle: Option<i64>,
        /// Centre the range on this cycle
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        at_cycle: Option<i64>,
        /// Number of cycles before/after --at-cycle
        #[arg(long, value_name = "K")]
        context: Option<i64>,
        /// Time/tick range A..B
        #[arg(long, value_name = "A..B")]
        time: Option<String>,
        /// Use a named query from [queries]
        #[arg(long, value_name = "NAME")]
        query: Option<String>,
    },

    /// Write a windowed copy of a dump as VCD (small fixture for a failure)
    Cut {
        /// Cycle range A..B (logical cycles of the resolved clock)
        #[arg(long, value_name = "A..B")]
        cycles: Option<String>,
        /// First cycle of the range
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        from_cycle: Option<i64>,
        /// Last cycle of the range
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        to_cycle: Option<i64>,
        /// Signals to keep (repeatable, comma separated, @group allowed; globs ok)
        #[arg(long, value_delimiter = ',', value_name = "NAME")]
        signal: Vec<String>,
        /// Write to FILE instead of stdout
        #[arg(short = 'o', long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Include every signal in the file (default when --signal is omitted)
        #[arg(long)]
        all: bool,
    },

    /// Scan shifts for the alignment with the fewest mismatches
    Align {
        /// Silver/reference signal (side A)
        #[arg(long, value_name = "SPEC")]
        a: String,
        /// DUT/optimized signal (side B)
        #[arg(long, value_name = "SPEC")]
        b: String,
        /// Compare units
        #[arg(long, value_enum, value_name = "cycles|transfers")]
        align: Option<AlignMode>,
        /// Treat |b - a| > N as a mismatch (numeric comparison against a bound)
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        max_delta: Option<i128>,
        /// Shift range A..=B
        #[arg(long, value_name = "A..=B")]
        shifts: Option<String>,
        /// Highest shift to try when --shifts is not given
        #[arg(long, value_name = "N")]
        max_shift: Option<u32>,
        /// First cycle to compare
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        from_cycle: Option<i64>,
        /// Last cycle to compare
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        to_cycle: Option<i64>,
        /// First time/tick to compare
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// Last time/tick to compare
        #[arg(long, value_name = "TIME")]
        to: Option<String>,
        /// Only compare cycles where this signal is 1 (cycles mode)
        #[arg(long, value_name = "NAME")]
        gate: Option<String>,
        /// A-side event valid (transfers mode)
        #[arg(long, value_name = "NAME")]
        in_valid: Option<String>,
        /// A-side event ready (transfers mode, optional)
        #[arg(long, value_name = "NAME")]
        in_ready: Option<String>,
        /// B-side event valid (transfers mode)
        #[arg(long, value_name = "NAME")]
        out_valid: Option<String>,
        /// B-side event ready (transfers mode, optional)
        #[arg(long, value_name = "NAME")]
        out_ready: Option<String>,
        /// Value comparison mode
        #[arg(long, value_enum, value_name = "bit-exact|numeric")]
        cmp: Option<CmpMode>,
        /// How x/z values are treated
        #[arg(long, value_enum, value_name = "mismatch|wildcard")]
        x: Option<XMode>,
    },

    /// Compare signal pairs and report divergent cycles/transfers
    Compare {
        /// Pair A=B or A=B:SHIFT (repeatable)
        #[arg(long = "map", value_name = "A=B[:SHIFT]")]
        map: Vec<String>,
        /// Named check from [checks]
        #[arg(long, value_name = "NAME")]
        check: Option<String>,
        /// Override the shift of every pair (including a --check's pair)
        #[arg(long, value_name = "N")]
        shift: Option<u32>,
        /// Pair every signal matching GLOB on side A with its same-leaf twin on
        /// the pattern's right-hand scope: --pair-scopes dut,ref --pair-by leaf
        #[arg(long, value_name = "PREFIX")]
        pair_scopes: Option<String>,
        /// Pairing rule for --pair-scopes (only `leaf` today)
        #[arg(long, value_name = "leaf")]
        pair_by: Option<String>,
        /// Pair all signals matching GLOB_A on side A with GLOB_B on side B,
        /// matched by leaf name: --map-glob 'dut.*=ref.*'
        #[arg(long, value_name = "GLOB_A=GLOB_B")]
        map_glob: Vec<String>,
        /// Add delta/bits_differ columns to divergence rows
        #[arg(long)]
        delta: bool,
        /// Treat |b - a| > N as a divergence (numeric comparison against a bound)
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        max_delta: Option<i128>,
        /// Which cycle the --probe columns are sampled at
        #[arg(
            long,
            value_enum,
            value_name = "a_cycle|b_cycle",
            default_value = "a-cycle"
        )]
        probe_at: ProbeAt,
        /// Pick the best shift per pair automatically
        #[arg(long)]
        auto_shift: bool,
        /// Compare units
        #[arg(long, value_enum, value_name = "cycles|transfers")]
        align: Option<AlignMode>,
        /// Only compare cycles where this signal is 1 (cycles mode)
        #[arg(long, value_name = "NAME")]
        gate: Option<String>,
        /// A-side event valid (transfers mode)
        #[arg(long, value_name = "NAME")]
        in_valid: Option<String>,
        /// A-side event ready (transfers mode, optional)
        #[arg(long, value_name = "NAME")]
        in_ready: Option<String>,
        /// B-side event valid (transfers mode)
        #[arg(long, value_name = "NAME")]
        out_valid: Option<String>,
        /// B-side event ready (transfers mode, optional)
        #[arg(long, value_name = "NAME")]
        out_ready: Option<String>,
        /// First cycle to compare
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        from_cycle: Option<i64>,
        /// Last cycle to compare
        #[arg(long, value_name = "N", allow_hyphen_values = true)]
        to_cycle: Option<i64>,
        /// First time/tick to compare
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// Last time/tick to compare
        #[arg(long, value_name = "TIME")]
        to: Option<String>,
        /// Value comparison mode
        #[arg(long, value_enum, value_name = "bit-exact|numeric")]
        cmp: Option<CmpMode>,
        /// How x/z values are treated
        #[arg(long, value_enum, value_name = "mismatch|wildcard")]
        x: Option<XMode>,
        /// Highest shift tried by --auto-shift
        #[arg(long, value_name = "N")]
        max_shift: Option<u32>,
        /// Extra signals to sample at each divergent cycle
        #[arg(long, value_delimiter = ',', value_name = "NAME")]
        probe: Vec<String>,
        /// Keep only the first divergent row
        #[arg(long)]
        first_only: bool,
        /// Drop rows whose values did not change exactly at an edge
        #[arg(long)]
        at_edges_only: bool,
    },

    /// Run a Lua script (FILE, or @NAME from [scripts]) against the dump
    #[command(long_about = SCRIPT_API_HELP)]
    Run {
        /// Script file, or @NAME for a [scripts] entry
        #[arg(value_name = "FILE|@NAME")]
        script: String,
        #[command(flatten)]
        opts: ScriptArgs,
    },

    /// Evaluate a Lua script given on the command line
    #[command(long_about = SCRIPT_API_HELP)]
    Eval {
        /// Lua source
        #[arg(value_name = "CODE")]
        code: String,
        #[command(flatten)]
        opts: ScriptArgs,
    },
}
