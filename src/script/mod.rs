//! Script mode: embedded Lua 5.4 (`waveq run` / `waveq eval`).
//!
//! A script sees the loaded dump (both sides when the user asked for two), a
//! frozen `wave` API ([`api`]) and nothing else: no `io`/`os`/`debug`/`package`,
//! an instruction budget and a memory limit. Rows the script emits go through
//! [`Doc`], so `--format text|json|jsonl` and the 0/1/2 exit-code contract keep
//! their documented meanings:
//!
//! * the script returns `nil`/`true` -> exit 0, `false` -> exit 1, anything
//!   else (or any script error) -> exit 2.

pub mod api;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Result, bail};
use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Value, VmState};
use serde_json::Value as Json;
use wellen::simple::Waveform;

use crate::clock::ClockInfo;
use crate::config::TypeTable;
use crate::format::Radix;
use crate::output::{Doc, Field, Format, Table};
use crate::resolve::{ResolveCtx, SignArgs};
use crate::sides::Side;

/// Budgets for one script run; `0` disables the corresponding limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptOpts {
    /// Instruction budget (`--budget`).
    pub budget: u64,
    /// Lua memory limit in MiB (`--memory`).
    pub memory: u64,
}

/// One logical side as a script sees it.
pub struct LuaSide {
    pub wf: Rc<RefCell<Waveform>>,
    /// `None` when the side has no clock (`--clock none`).
    pub clock: Option<ClockInfo>,
    /// Local edge index that represents logical cycle 0.
    pub origin: i64,
    pub path: PathBuf,
}

impl LuaSide {
    /// Logical cycle -> local (file) cycle index.
    pub fn local_of(&self, logical: i64) -> i64 {
        logical + self.origin
    }
}

/// Output a script produced, in call order.
#[derive(Default)]
pub struct ScriptOut {
    /// One normalized record per `wave.emit` call.
    pub rows: Vec<Json>,
    /// Column order: `wave.columns{...}`, or the first record's keys.
    pub columns: Option<Vec<String>>,
    pub trailer: Vec<(String, Json)>,
}

/// Everything a `wave.*` call can reach.
pub struct ScriptCtx {
    /// Indexed by `Side`: `[Side::A as usize]`, `[Side::B as usize]`.
    pub sides: [LuaSide; 2],
    /// Whether the invocation asked for two sides (side B may still share A's
    /// dump, e.g. `--b-cycle-offset`).
    pub has_b: bool,
    pub types: TypeTable,
    pub sign: SignArgs,
    pub radix: Option<Radix>,
    pub format: Format,
    pub quiet: bool,
    pub out: RefCell<ScriptOut>,
}

impl ScriptCtx {
    pub fn side(&self, side: Side) -> &LuaSide {
        &self.sides[side_index(side)]
    }

    pub fn resolve_ctx(&self) -> ResolveCtx<'_> {
        ResolveCtx {
            types: &self.types,
            sign: &self.sign,
            radix: self.radix,
        }
    }
}

pub fn side_index(side: Side) -> usize {
    match side {
        Side::A => 0,
        Side::B => 1,
    }
}

/// Text cell for one JSON value: `-` for `null`, compact JSON for containers.
pub fn cell_text(value: &Json) -> String {
    match value {
        Json::Null => "-".to_string(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => n
            .as_i64()
            .map(|i| i.to_string())
            .or_else(|| n.as_u64().map(|u| u.to_string()))
            .unwrap_or_else(|| n.to_string()),
        Json::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Column names of the first emitted record, sorted.
pub fn record_columns(record: &Json) -> Vec<String> {
    let mut keys: Vec<String> = record
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

/// Build the sandboxed VM, run `source` and turn its output into a [`Doc`].
///
/// `label` is the chunk name (`@path` for a file, `=eval` for inline code) and
/// names the source in error messages.
pub fn run_chunk(ctx: Rc<ScriptCtx>, source: &str, label: &str, opts: &ScriptOpts) -> Result<Doc> {
    let libs = StdLib::ALL_SAFE & !(StdLib::IO | StdLib::OS | StdLib::PACKAGE);
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    // `require`/`loadfile`/`dofile` would reach the file system; `load_*` and
    // `loadstring` only load strings and stay available.
    for name in ["dofile", "loadfile", "require"] {
        lua.globals().set(name, Value::Nil)?;
    }

    if opts.memory > 0 {
        let bytes = usize::try_from(opts.memory.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX);
        match lua.set_memory_limit(bytes) {
            Ok(_) => {}
            Err(mlua::Error::MemoryControlNotAvailable) => {
                if !ctx.quiet {
                    eprintln!("waveq: this Lua build has no memory control; --memory ignored");
                }
            }
            Err(err) => return Err(anyhow::anyhow!("{err}")),
        }
    }

    if opts.budget > 0 {
        // Hook overhead grows with 1/stride; 1000 keeps it negligible while
        // still bounding a runaway loop to a fraction of a second.
        let stride = opts.budget.clamp(1, 1000) as u32;
        let budget = opts.budget;
        let calls = Cell::new(0u64);
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(stride),
            move |_lua, _debug| {
                let n = calls.get() + 1;
                calls.set(n);
                if n * u64::from(stride) > budget {
                    Err(mlua::Error::runtime(format!(
                        "instruction budget exceeded (--budget {budget})"
                    )))
                } else {
                    Ok(VmState::Continue)
                }
            },
        )?;
    }

    api::register(&lua, &ctx)?;

    let result: Value = lua
        .load(source)
        .set_name(label)
        .eval()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let failed = match &result {
        Value::Nil | Value::Boolean(true) => false,
        Value::Boolean(false) => true,
        other => bail!(
            "{label}: the script returned a {}; return nil, true or false",
            other.type_name()
        ),
    };

    let path_a = ctx.sides[0].path.display().to_string();
    let mut doc = Doc::new()
        .field(Field::s("file", path_a))
        .field(Field::s("script", label))
        .field(Field::s("sides", if ctx.has_b { "ab" } else { "a" }));

    {
        let out = ctx.out.borrow();
        if !out.rows.is_empty() {
            let columns = out
                .columns
                .clone()
                .unwrap_or_else(|| record_columns(&out.rows[0]));
            let mut table = Table::new(columns.clone());
            for row in &out.rows {
                let obj = row.as_object();
                let mut text = Vec::with_capacity(columns.len());
                let mut record = serde_json::Map::new();
                for column in &columns {
                    let value = obj
                        .and_then(|o| o.get(column))
                        .cloned()
                        .unwrap_or(Json::Null);
                    text.push(cell_text(&value));
                    record.insert(column.clone(), value);
                }
                table.push(text, Json::Object(record));
            }
            doc = doc.table(table);
        }
        for (key, value) in &out.trailer {
            doc = doc.trailer_field(Field::text_value(key, cell_text(value), value.clone()));
        }
    }

    if failed {
        doc = doc.header_record(true).exit(1);
    }
    Ok(doc)
}
