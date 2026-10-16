//! The `wave` API: the frozen surface scripts are written against.
//!
//! Cycle arguments are always *logical* cycles (the tool-wide convention: local
//! cycle minus the side's origin). Every function that can be pointed at a side
//! takes an optional trailing `"a"`/`"b"`; `"b"` without a B side errors.

use std::io::Write;
use std::rc::Rc;

use mlua::{
    Function, Lua, LuaSerdeExt, Table, UserData, UserDataFields, UserDataMethods, Value, Variadic,
};
use serde_json::{Map, Value as Json};
use wellen::simple::Waveform;

use crate::clock::ClockInfo;
use crate::format::{Radix, format_time, timescale_text};
use crate::output::Format;
use crate::resolve::{self, SigHandle};
use crate::sample::{Cell, Val};
use crate::sides::{self, Side};

use super::{LuaSide, ScriptCtx, record_columns};

/// Register the `wave` table and replace `print`.
pub(crate) fn register(lua: &Lua, ctx: &Rc<ScriptCtx>) -> anyhow::Result<()> {
    let wave = lua.create_table()?;
    install_globals(lua, &wave, ctx)?;
    lua.globals().set("wave", wave)?;
    install_print(lua, ctx)?;
    Ok(())
}

fn install_globals(lua: &Lua, wave: &Table, ctx: &Rc<ScriptCtx>) -> anyhow::Result<()> {
    {
        let ctx = ctx.clone();
        wave.set(
            "sides",
            lua.create_function(move |lua, ()| {
                let sides: Vec<&str> = if ctx.has_b { vec!["a", "b"] } else { vec!["a"] };
                lua.to_value(&sides)
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "path",
            lua.create_function(move |lua, side: Option<String>| {
                let side = parse_side(&ctx, side.as_deref())?;
                Ok(Json::from(ctx.side(side).path.display().to_string()))
                    .and_then(|v| lua.to_value(&v))
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "info",
            lua.create_function(move |lua, ()| {
                let wf = ctx.sides[0].wf.borrow();
                let value = info_value(&wf, &ctx.sides[0].path.display().to_string());
                lua.to_value(&value)
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "clock",
            lua.create_function(move |lua, side: Option<String>| {
                let side = parse_side(&ctx, side.as_deref())?;
                let Some(info) = ctx.side(side).clock.clone() else {
                    return Ok(Value::Nil);
                };
                let ts = ts_of(&ctx.side(side).wf.borrow());
                let value = info.summary_json(ts);
                lua.to_value(&value)
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "cycles",
            lua.create_function(move |lua, side: Option<String>| {
                let side = parse_side(&ctx, side.as_deref())?;
                let info = clock_of(&ctx, side, "wave.cycles")?;
                lua.to_value(&Json::from(info.edges.len()))
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "last_cycle",
            lua.create_function(move |lua, side: Option<String>| {
                let side = parse_side(&ctx, side.as_deref())?;
                let info = clock_of(&ctx, side, "wave.last_cycle")?;
                lua.to_value(&Json::from(info.last_cycle()))
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "tick",
            lua.create_function(move |lua, (cycle, side): (i64, Option<String>)| {
                let side = parse_side(&ctx, side.as_deref())?;
                let info = clock_of(&ctx, side, "wave.tick")?;
                if cycle < 0 || cycle > info.last_cycle() {
                    return Ok(Value::Nil);
                }
                match info.tick_of(ctx.side(side).local_of(cycle)) {
                    Some(tick) => lua.to_value(&Json::from(tick)),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "time",
            lua.create_function(move |lua, tick: i64| {
                let ts = ts_of(&ctx.sides[0].wf.borrow());
                match u64::try_from(tick) {
                    Ok(tick) => lua.to_value(&Json::from(format_time(tick, ts))),
                    Err(_) => Err(mlua::Error::runtime("wave.time: tick must be >= 0")),
                }
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "signal",
            lua.create_function(move |lua, (spec, side): (String, Option<String>)| {
                let side = parse_side(&ctx, side.as_deref())?;
                let sig = build_signal(&ctx, &spec, side)?;
                lua.create_userdata(sig)
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "signals",
            lua.create_function(
                move |lua, (pattern, side): (Option<String>, Option<String>)| {
                    let side = parse_side(&ctx, side.as_deref())?;
                    let wf = ctx.side(side).wf.borrow();
                    let h = wf.hierarchy();
                    let mut records: Vec<Json> = Vec::new();
                    for v in resolve::iter_vars(h) {
                        let full_name = resolve::var_full_name(h, v);
                        if let Some(pattern) = pattern.as_deref()
                            && !pattern.is_empty()
                            && !resolve::matches_any(&[pattern.to_string()], &full_name)?
                        {
                            continue;
                        }
                        records.push(crate::cmd::introspect::signal_json(
                            h, v, &ctx.sign, &ctx.types,
                        ));
                    }
                    lua.to_value(&Json::Array(records))
                },
            )?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "emit",
            lua.create_function(move |lua, record: Value| {
                let value: Json = lua.from_value(record)?;
                emit(&ctx, value)
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "columns",
            lua.create_function(move |lua, columns: Value| {
                let value: Json = lua.from_value(columns)?;
                let Json::Array(items) = value else {
                    return Err(mlua::Error::runtime(
                        "wave.columns: expected a table of column names",
                    ));
                };
                let mut names = Vec::with_capacity(items.len());
                for item in items {
                    let Json::String(name) = item else {
                        return Err(mlua::Error::runtime(
                            "wave.columns: expected a table of column names",
                        ));
                    };
                    names.push(name);
                }
                if names.is_empty() {
                    return Err(mlua::Error::runtime(
                        "wave.columns: expected at least one column name",
                    ));
                }
                let mut out = ctx.out.borrow_mut();
                if !out.rows.is_empty() {
                    return Err(mlua::Error::runtime(
                        "wave.columns: columns are already fixed by the first wave.emit",
                    ));
                }
                out.columns = Some(names);
                Ok(())
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "trailer",
            lua.create_function(move |lua, (key, value): (String, Value)| {
                let value: Json = lua.from_value(value)?;
                ctx.out.borrow_mut().trailer.push((key, value));
                Ok(())
            })?,
        )?;
    }
    {
        let ctx = ctx.clone();
        wave.set(
            "note",
            lua.create_function(move |_, msg: String| {
                if !ctx.quiet {
                    eprintln!("{msg}");
                }
                Ok(())
            })?,
        )?;
    }
    {
        wave.set(
            "encode_json",
            lua.create_function(move |lua, value: Value| {
                let value: Json = lua.from_value(value)?;
                serde_json::to_string(&value)
                    .map_err(|e| mlua::Error::runtime(format!("wave.encode_json: {e}")))
            })?,
        )?;
    }
    {
        wave.set(
            "decode_json",
            lua.create_function(move |lua, text: String| {
                let value: Json = serde_json::from_str(&text)
                    .map_err(|e| mlua::Error::runtime(format!("wave.decode_json: {e}")))?;
                lua.to_value(&value)
            })?,
        )?;
    }
    {
        wave.set(
            "read_json",
            lua.create_function(move |lua, path: String| {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| mlua::Error::runtime(format!("wave.read_json {path}: {e}")))?;
                let value: Json = serde_json::from_str(&text)
                    .map_err(|e| mlua::Error::runtime(format!("wave.read_json {path}: {e}")))?;
                lua.to_value(&value)
            })?,
        )?;
    }
    Ok(())
}

/// `print` joins its arguments with tabs and honours `__tostring`; it goes to
/// stdout for `--format text` and to stderr otherwise, so `--format json|jsonl`
/// output stays one parseable document.
fn install_print(lua: &Lua, ctx: &Rc<ScriptCtx>) -> anyhow::Result<()> {
    let ctx = ctx.clone();
    let print = lua.create_function(move |lua, args: Variadic<Value>| {
        let tostring: Function = lua.globals().get("tostring")?;
        let mut parts = Vec::with_capacity(args.len());
        for arg in args {
            let text: String = tostring.call(arg)?;
            parts.push(text);
        }
        let line = parts.join("\t");
        if ctx.format == Format::Text {
            let _ = writeln!(std::io::stdout(), "{line}");
        } else {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
        Ok(())
    })?;
    lua.globals().set("print", print)?;
    Ok(())
}

/// Append one `wave.emit` record, fixing the column list on first use.
fn emit(ctx: &ScriptCtx, value: Json) -> mlua::Result<()> {
    let Json::Object(record) = value else {
        return Err(mlua::Error::runtime(
            "wave.emit: expected a table with named columns",
        ));
    };
    let mut out = ctx.out.borrow_mut();
    let columns = match &out.columns {
        Some(columns) => columns.clone(),
        None => {
            let columns = record_columns(&Json::Object(record.clone()));
            out.columns = Some(columns.clone());
            columns
        }
    };
    if let Some(unknown) = record.keys().find(|k| !columns.contains(k)) {
        return Err(mlua::Error::runtime(format!(
            "wave.emit: unknown column \"{unknown}\" (columns: {})",
            columns.join(", ")
        )));
    }
    let mut row = Map::new();
    for column in &columns {
        row.insert(
            column.clone(),
            record.get(column).cloned().unwrap_or(Json::Null),
        );
    }
    out.rows.push(Json::Object(row));
    Ok(())
}

fn parse_side(ctx: &ScriptCtx, arg: Option<&str>) -> mlua::Result<Side> {
    match arg {
        None | Some("a") => Ok(Side::A),
        Some("b") if ctx.has_b => Ok(Side::B),
        Some("b") => Err(mlua::Error::runtime(
            "no side B: pass --b-file, --b-cycle-offset or --b-anchor",
        )),
        Some(other) => Err(mlua::Error::runtime(format!(
            "side must be \"a\" or \"b\", got \"{other}\""
        ))),
    }
}

fn clock_of<'a>(ctx: &'a ScriptCtx, side: Side, what: &str) -> mlua::Result<&'a ClockInfo> {
    ctx.side(side)
        .clock
        .as_ref()
        .ok_or_else(|| mlua::Error::runtime(format!("{what}: no clock (--clock none)")))
}

fn ts_of(wf: &Waveform) -> Option<wellen::Timescale> {
    wf.hierarchy().timescale()
}

fn info_value(wf: &Waveform, path: &str) -> Json {
    let h = wf.hierarchy();
    let ts = h.timescale();
    let tt = wf.time_table();
    let mut m = Map::new();
    m.insert("file".into(), Json::from(path));
    m.insert(
        "format".into(),
        Json::from(crate::load::format_name(h.file_format())),
    );
    m.insert("version".into(), Json::from(h.version().to_string()));
    m.insert("date".into(), Json::from(h.date().to_string()));
    m.insert("timescale".into(), Json::from(timescale_text(ts)));
    m.insert(
        "scopes".into(),
        Json::from(
            h.all_scopes()
                .filter(|s| !h[*s].full_name(h).is_empty())
                .count(),
        ),
    );
    m.insert("vars".into(), Json::from(h.all_vars().count()));
    m.insert("tick_count".into(), Json::from(tt.len()));
    if let (Some(first), Some(last)) = (tt.first(), tt.last()) {
        m.insert("first_tick".into(), Json::from(*first));
        m.insert("first_time".into(), Json::from(format_time(*first, ts)));
        m.insert("last_tick".into(), Json::from(*last));
        m.insert("last_time".into(), Json::from(format_time(*last, ts)));
    }
    Json::Object(m)
}

fn parse_radix(name: &str) -> mlua::Result<Radix> {
    match name {
        "auto" => Ok(Radix::Auto),
        "bin" => Ok(Radix::Bin),
        "hex" => Ok(Radix::Hex),
        "oct" => Ok(Radix::Oct),
        "dec" => Ok(Radix::Dec),
        other => Err(mlua::Error::runtime(format!(
            "invalid radix \"{other}\": use auto|bin|hex|oct|dec"
        ))),
    }
}

/// Resolve `spec` on `side` and load it into the owning waveform.
///
/// The `RefCell` is released before the userdata exists: no borrow is ever held
/// across a nested Lua call.
fn build_signal(ctx: &Rc<ScriptCtx>, spec: &str, side: Side) -> mlua::Result<LuaSig> {
    let handle = {
        let wf_a = ctx.sides[0].wf.borrow();
        let wf_b = if side == Side::B {
            Some(ctx.sides[1].wf.borrow())
        } else {
            None
        };
        let h = sides::hierarchy_of(&wf_a, wf_b.as_deref()).hierarchy();
        resolve::resolve_sig(h, &ctx.resolve_ctx(), spec)
            .map_err(|e| mlua::Error::runtime(format!("{e}")))?
            .on_side(side)
    };
    ctx.side(side)
        .wf
        .borrow_mut()
        .load_signals_multi_threaded(&[handle.signal_ref]);
    Ok(LuaSig {
        ctx: ctx.clone(),
        side,
        handle,
    })
}

/// One resolved signal as a Lua userdata.
pub struct LuaSig {
    ctx: Rc<ScriptCtx>,
    side: Side,
    handle: SigHandle,
}

impl LuaSig {
    fn side(&self) -> &LuaSide {
        self.ctx.side(self.side)
    }

    fn kind(&self) -> &'static str {
        if self.handle.is_real() {
            "real"
        } else if self.handle.is_string() {
            "string"
        } else {
            "bitvec"
        }
    }

    /// Cell at a logical cycle, plus the tick it was sampled at.
    fn cell_at(&self, logical: i64) -> Option<(Cell, u64)> {
        let side = self.side();
        let tick = side.clock.as_ref()?.tick_of(side.local_of(logical))?;
        let wf = side.wf.borrow();
        Some((self.handle.cell_at_tick(&wf, tick), tick))
    }

    /// `{...}` record of one sample, or `nil` when the signal has no value
    /// there.
    fn sample(&self, logical: i64, cycle: Option<i64>, what: &str) -> mlua::Result<Option<Json>> {
        clock_of(&self.ctx, self.side, what)?;
        let Some((cell, tick)) = self.cell_at(logical) else {
            return Ok(None);
        };
        if cell.val.is_none() {
            return Ok(None);
        }
        let ts = ts_of(&self.side().wf.borrow());
        let mut record = Map::new();
        if let Some(cycle) = cycle {
            record.insert("cycle".into(), Json::from(cycle));
        }
        record.insert("tick".into(), Json::from(tick));
        record.insert("time".into(), Json::from(format_time(tick, ts)));
        let values = crate::cmd::read::signal_value_json(&cell, &self.handle);
        for (key, value) in values.as_object().cloned().unwrap_or_default() {
            record.insert(key, value);
        }
        Ok(Some(Json::Object(record)))
    }

    fn bits_at(&self, logical: i64, what: &str) -> mlua::Result<Option<String>> {
        clock_of(&self.ctx, self.side, what)?;
        match self.cell_at(logical) {
            Some((cell, _)) => Ok(cell.bits().map(str::to_string)),
            None => Ok(None),
        }
    }
}

fn lua_err(err: anyhow::Error) -> mlua::Error {
    mlua::Error::runtime(format!("{err}"))
}

impl UserData for LuaSig {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("name", |_, this| Ok(this.handle.full_name.clone()));
        fields.add_field_method_get("spec", |_, this| Ok(this.handle.spec.clone()));
        fields.add_field_method_get("width", |_, this| Ok(this.handle.eff_width()));
        fields.add_field_method_get("signed", |_, this| Ok(this.handle.signed));
        fields.add_field_method_get("radix", |_, this| Ok(this.handle.radix.name()));
        fields.add_field_method_get("kind", |_, this| Ok(this.kind()));
        fields.add_field_method_get("direction", |_, this| Ok(this.handle.direction));
        fields.add_field_method_get("index", |_, this| {
            Ok(this.handle.index.map(|(msb, lsb)| format!("{msb}:{lsb}")))
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("at", |lua, this, cycle: i64| {
            match this.sample(cycle, Some(cycle), "signal:at")? {
                Some(record) => lua.to_value(&record),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("at_tick", |lua, this, tick: i64| {
            let Ok(tick) = u64::try_from(tick) else {
                return Ok(Value::Nil);
            };
            let wf = this.side().wf.borrow();
            let cell = this.handle.cell_at_tick(&wf, tick);
            if cell.val.is_none() {
                return Ok(Value::Nil);
            }
            let ts = ts_of(&wf);
            let mut record = Map::new();
            record.insert("tick".into(), Json::from(tick));
            record.insert("time".into(), Json::from(format_time(tick, ts)));
            let values = crate::cmd::read::signal_value_json(&cell, &this.handle);
            for (key, value) in values.as_object().cloned().unwrap_or_default() {
                record.insert(key, value);
            }
            lua.to_value(&Json::Object(record))
        });
        methods.add_method("bits", |lua, this, cycle: i64| {
            match this.bits_at(cycle, "signal:bits")? {
                Some(bits) => lua.to_value(&Json::from(bits)),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method(
            "value",
            |lua, this, (cycle, radix): (i64, Option<String>)| {
                clock_of(&this.ctx, this.side, "signal:value")?;
                let radix = match radix.as_deref() {
                    Some(name) => parse_radix(name)?,
                    None => this.handle.radix,
                };
                let Some((cell, _)) = this.cell_at(cycle) else {
                    return Ok(Value::Nil);
                };
                let Some(value) = cell.val.as_ref() else {
                    return Ok(Value::Nil);
                };
                let text = value.render(radix, this.handle.signed).map_err(lua_err)?;
                lua.to_value(&Json::from(text))
            },
        );
        methods.add_method("int", |lua, this, cycle: i64| {
            let Some(bits) = this.bits_at(cycle, "signal:int")? else {
                return Ok(Value::Nil);
            };
            let text =
                crate::format::render(&bits, Radix::Dec, this.handle.signed).map_err(lua_err)?;
            if text == "x" {
                return Ok(Value::Nil);
            }
            match text.parse::<i64>() {
                Ok(value) => lua.to_value(&Json::from(value)),
                Err(_) => Err(mlua::Error::runtime(format!(
                    "{}: {text} does not fit a Lua integer; use :value() or :bits()",
                    this.handle.full_name
                ))),
            }
        });
        methods.add_method("is_xz", |lua, this, cycle: i64| {
            clock_of(&this.ctx, this.side, "signal:is_xz")?;
            let is_xz = match this.cell_at(cycle) {
                Some((cell, _)) => cell.val.as_ref().is_some_and(|v| v.has_xz()),
                None => false,
            };
            lua.to_value(&Json::from(is_xz))
        });
        methods.add_method(
            "changes",
            |lua, this, (from, to): (Option<i64>, Option<i64>)| {
                let side = this.side();
                let info = clock_of(&this.ctx, this.side, "signal:changes")?;
                let wf = side.wf.borrow();
                let mut rows: Vec<Json> = Vec::new();
                if let Some(sig) = this.handle.signal(&wf) {
                    let tt = wf.time_table();
                    for (idx, value) in sig.iter_changes() {
                        let tick = tt[idx as usize];
                        let cycle = info.cycle_of(tick) - side.origin;
                        if from.is_some_and(|a| cycle < a) || to.is_some_and(|b| cycle > b) {
                            continue;
                        }
                        let val = sliced(value, &this.handle);
                        let mut record = Map::new();
                        record.insert("cycle".into(), Json::from(cycle));
                        record.insert("tick".into(), Json::from(tick));
                        record.insert("bits".into(), val.json_bits());
                        record.insert(
                            "value".into(),
                            Json::from(
                                val.render(this.handle.radix, this.handle.signed)
                                    .unwrap_or_default(),
                            ),
                        );
                        record.insert("has_x".into(), Json::from(val.has_xz()));
                        rows.push(Json::Object(record));
                    }
                }
                lua.to_value(&Json::Array(rows))
            },
        );
    }
}

/// Apply the handle's `[msb:lsb]` slice to a raw change value.
fn sliced(value: wellen::SignalValueRef<'_>, handle: &SigHandle) -> Val {
    let val = Val::from_ref(value);
    match val {
        Val::Bits(bits) => Val::Bits(handle.apply_slice(&bits).unwrap_or(bits)),
        other => other,
    }
}
