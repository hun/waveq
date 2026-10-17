//! Waveform loading and user-facing error mapping.

use anyhow::Result;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use wellen::simple::Waveform;
use wellen::{FileFormat, LoadOptions, WellenError, viewers};

pub fn load_options() -> LoadOptions {
    LoadOptions {
        multi_thread: true,
        remove_scopes_with_empty_name: false,
    }
}

/// Header-only load: hierarchy, format, timescale, body length.
pub fn header(path: &Path, opts: &LoadOptions) -> Result<viewers::HeaderResult<BufReader<File>>> {
    check_readable(path)?;
    viewers::read_header_from_file(path, opts).map_err(|e| map_err(path, e))
}

/// Full load (header + body); signals are still loaded on demand.
pub fn full(path: &Path, opts: &LoadOptions) -> Result<Waveform> {
    check_readable(path)?;
    wellen::simple::read_with_options(path, opts).map_err(|e| map_err(path, e))
}

/// wellen's format detection opens the file with `File::open(..).expect(..)`
/// *before* it can return a `WellenError`, so a missing or unreadable path
/// aborts the process instead of reporting an error. Probe the path ourselves
/// first: directories pass through (`File::open` succeeds) and are reported as
/// an unknown format by wellen, as before.
fn check_readable(path: &Path) -> Result<()> {
    File::open(path)
        .map(|_| ())
        .map_err(|e| map_err(path, WellenError::Io(e)))
}

pub fn format_name(f: FileFormat) -> &'static str {
    match f {
        FileFormat::Vcd => "vcd",
        FileFormat::Fst => "fst",
        FileFormat::Ghw => "ghw",
        FileFormat::Unknown => "unknown",
    }
}

/// Translate a wellen error into an actionable message.
pub fn map_err(path: &Path, err: WellenError) -> anyhow::Error {
    match err {
        WellenError::UnknownFileFormat => anyhow::anyhow!(
            "unknown file format: {}\n\
             waveq reads VCD, FST and GHW. To get one of those:\n\
             \x20 Vivado/XSim : open_vcd dump.vcd ; log_vcd * ; run all ; close_vcd      (Tcl console)\n\
             \x20 ModelSim    : vcd file dump.vcd ; vcd add -r /tb/* ; run -all\n\
             \x20 GHDL        : ghdl -r tb --wave=dump.ghw\n\
             \x20 conversion  : vcd2fst dump.vcd dump.fst\n\
             \x20 (WDB/WLF and Verdi FSDB are closed formats and are not read by waveq.)",
            path.display()
        ),
        WellenError::FailedToLoad(format, msg) => anyhow::anyhow!(
            "failed to load {} as {}: {}",
            path.display(),
            format_name(format),
            msg.trim()
        ),
        WellenError::Io(e) => anyhow::anyhow!("io error reading {}: {e}", path.display()),
    }
}

/// Resolve the wave file path with the documented precedence.
pub fn resolve_path_opt(cli: Option<&PathBuf>, config: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = cli {
        return Some(p.clone());
    }
    config.map(PathBuf::from)
}
