//! Introspection commands: `info`, `scopes`, `signals`, `signal`, `clock`, `config`.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use wellen::{Hierarchy, ScopeRef, VarRef};

use crate::clock::{self, ClockInfo};
use crate::format::{format_time, timescale_text, timscale_unit_name};
use crate::output::{Doc, Field, Table};
use crate::resolve::{self, direction_name, parse_sigspec, var_type_name};
use crate::sample;
use crate::{load, sample::Cell};

use super::{App, cycle_json, cycle_str, keep_signal, ts_of};

fn scope_full_name(h: &Hierarchy, s: ScopeRef) -> String {
    h[s].full_name(h)
}

fn depth_of(full_name: &str) -> usize {
    if full_name.is_empty() {
        0
    } else {
        full_name.matches('.').count()
    }
}

fn encoding_text(h: &Hierarchy, v: VarRef) -> String {
    match h[v].signal_encoding(h) {
        wellen::SignalEncoding::BitVector(n) => format!("bitvector({n})"),
        wellen::SignalEncoding::Real => "real".to_string(),
        wellen::SignalEncoding::String => "string".to_string(),
    }
}

fn timescale_json(ts: Option<wellen::Timescale>) -> Value {
    match ts {
        None => Value::Null,
        Some(ts) => json!({
            "factor": ts.factor,
            "unit": timscale_unit_name(ts.unit),
        }),
    }
}

/// Best-effort clock resolution for commands that merely want cycles when
/// available; an explicit `--clock` failure is still fatal at the call site.
pub fn optional_clock(app: &App, wf: &mut wellen::simple::Waveform) -> Option<ClockInfo> {
    match clock::resolve_clock(wf, app.clock_spec.as_deref(), app.edge, app.quiet) {
        Ok(c) => c,
        Err(e) => {
            app.note(&format!("clock: {e}"));
            None
        }
    }
}

pub fn info(app: &App, full: bool, with_clock: bool) -> Result<Doc> {
    let path = app.require_file()?;
    let opts = load::load_options();
    let mut doc = Doc::new();

    if !full && !with_clock {
        let hd = load::header(path, &opts)?;
        let h = &hd.hierarchy;
        let ts = ts_of(h);
        doc = doc
            .field(Field::s("file", path.display().to_string()))
            .field(Field::s("format", load::format_name(hd.file_format)))
            .field(Field::s("version", h.version().to_string()))
            .field(Field::s("date", h.date().to_string()))
            .field(Field::text_value(
                "timescale",
                timescale_text(ts),
                timescale_json(ts),
            ))
            .field(Field::u(
                "scopes",
                h.all_scopes()
                    .filter(|s| !scope_full_name(h, *s).is_empty())
                    .count() as u64,
            ))
            .field(Field::u("vars", h.all_vars().count() as u64))
            .field(Field::u("body_bytes", hd.body_len));
        return Ok(doc);
    }

    let mut wf = load::full(path, &opts)?;
    {
        let h = wf.hierarchy();
        let ts = ts_of(h);
        let tt = wf.time_table();
        doc = doc
            .field(Field::s("file", path.display().to_string()))
            .field(Field::s("format", load::format_name(h.file_format())))
            .field(Field::s("version", h.version().to_string()))
            .field(Field::s("date", h.date().to_string()))
            .field(Field::text_value(
                "timescale",
                timescale_text(ts),
                timescale_json(ts),
            ))
            .field(Field::u(
                "scopes",
                h.all_scopes()
                    .filter(|s| !scope_full_name(h, *s).is_empty())
                    .count() as u64,
            ))
            .field(Field::u("vars", h.all_vars().count() as u64))
            .field(Field::u("tick_count", tt.len() as u64));
        if let (Some(first), Some(last)) = (tt.first(), tt.last()) {
            doc = doc
                .field(Field::u("first_tick", *first))
                .field(Field::s("first_time", format_time(*first, ts)))
                .field(Field::u("last_tick", *last))
                .field(Field::s("last_time", format_time(*last, ts)));
        }
    }
    if with_clock {
        let ts = ts_of(wf.hierarchy());
        match clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet) {
            Ok(Some(info)) => {
                doc = doc.field(Field::text_value(
                    "clock",
                    info.summary_text(ts),
                    info.summary_json(ts),
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(doc)
}

pub fn scopes(app: &App, matches: &[String], depth: Option<usize>) -> Result<Doc> {
    let path = app.require_file()?;
    let hd = load::header(path, &load::load_options())?;
    let h = &hd.hierarchy;
    let max_rows = app.max_rows(500);

    let mut columns = vec![
        "full_name".to_string(),
        "depth".to_string(),
        "type".to_string(),
        "vars".to_string(),
        "scopes".to_string(),
    ];
    if let Some(d) = depth {
        columns[1] = format!("depth<={d}");
    }
    let mut table = Table::new(columns);

    let all: Vec<ScopeRef> = h
        .all_scopes()
        .filter(|s| !scope_full_name(h, *s).is_empty())
        .collect();
    let mut kept = 0usize;
    for s in all {
        let full_name = scope_full_name(h, s);
        let d = depth_of(&full_name);
        if let Some(limit) = depth
            && d > limit
        {
            continue;
        }
        if !matches.is_empty() && !resolve::matches_any(matches, &full_name)? {
            continue;
        }
        kept += 1;
        let vars = h[s].vars(h).count();
        let scopes = h[s].scopes(h).count();
        let tpe = format!("{:?}", h[s].scope_type()).to_lowercase();
        let row_json = json!({
            "full_name": full_name,
            "depth": d,
            "type": tpe,
            "vars": vars,
            "scopes": scopes,
        });
        table.push(
            vec![
                full_name.clone(),
                d.to_string(),
                tpe,
                vars.to_string(),
                scopes.to_string(),
            ],
            row_json,
        );
    }
    let truncated = truncate(&mut table, max_rows);
    let mut doc = Doc::new()
        .field(Field::s("file", path.display().to_string()))
        .field(Field::u("scopes", kept as u64))
        .table(table);
    if truncated > 0 {
        doc = doc
            .trailer_field(Field::b("truncated", true))
            .trailer_field(Field::u("omitted", truncated as u64));
        app.note(&format!(
            "scopes: truncated to {} rows (use --max-rows 0 for all)",
            max_rows.unwrap_or(kept)
        ));
    }
    Ok(doc)
}

fn truncate(table: &mut Table, max_rows: Option<usize>) -> usize {
    match max_rows {
        Some(max) if table.rows.len() > max => {
            let omitted = table.rows.len() - max;
            table.rows.truncate(max);
            omitted
        }
        _ => 0,
    }
}

pub fn signals(
    app: &App,
    matches: &[String],
    scope: Option<&str>,
    count_only: bool,
    tree: bool,
) -> Result<Doc> {
    let path = app.require_file()?;
    let hd = load::header(path, &load::load_options())?;
    let h = &hd.hierarchy;
    let sign = app.sign_args(h)?;
    let max_rows = app.max_rows(500);
    if tree && !count_only {
        return signals_tree(app, h, matches, scope, max_rows);
    }

    let mut table = Table::new(vec![
        "full_name".to_string(),
        "width".to_string(),
        "var_type".to_string(),
        "direction".to_string(),
        "encoding".to_string(),
        "signed".to_string(),
        "index".to_string(),
    ]);

    let mut kept = 0usize;
    for v in resolve::iter_vars(h) {
        let full_name = resolve::var_full_name(h, v);
        if !keep_signal(matches, scope, &full_name)? {
            continue;
        }
        kept += 1;
        let width = h[v].length(h);
        let encoding = encoding_text(h, v);
        let (signed, source) = resolve::signedness(h, v, &full_name, &full_name, &sign, &app.types);
        let index = h[v]
            .index()
            .map(|i| format!("{}:{}", i.msb(), i.lsb()))
            .unwrap_or_else(|| "-".to_string());
        let row_json = json!({
            "full_name": full_name,
            "width": width,
            "var_type": var_type_name(h[v].var_type()),
            "direction": direction_name(h[v].direction()),
            "encoding": encoding,
            "signed": signed,
            "signed_source": source.name(),
            "index": h[v].index().map(|i| format!("{}:{}", i.msb(), i.lsb())),
        });
        table.push(
            vec![
                full_name,
                width
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                var_type_name(h[v].var_type()).to_string(),
                direction_name(h[v].direction()).to_string(),
                encoding,
                signed.to_string(),
                index,
            ],
            row_json,
        );
    }

    if count_only {
        return Ok(Doc::new()
            .field(Field::s("file", path.display().to_string()))
            .field(Field::u("count", kept as u64)));
    }
    let truncated = truncate(&mut table, max_rows);
    let mut doc = Doc::new()
        .field(Field::s("file", path.display().to_string()))
        .field(Field::u("count", kept as u64))
        .table(table);
    if truncated > 0 {
        doc = doc
            .trailer_field(Field::b("truncated", true))
            .trailer_field(Field::u("omitted", truncated as u64));
        app.note(&format!(
            "signals: truncated to {} rows (use --max-rows 0 for all)",
            max_rows.unwrap_or(kept)
        ));
    }
    Ok(doc)
}

pub fn signal(app: &App, name: &str, bits: Option<&str>) -> Result<Doc> {
    let path = app.require_file()?;
    let spec = match bits {
        Some(range) => {
            let mut s = parse_sigspec(range).map_err(|e| anyhow::anyhow!("--bits: {e}"))?;
            if s.bits.is_none() {
                bail!("--bits expects MSB:LSB (e.g. 15:8), got \"{range}\"");
            }
            s.name = name.to_string();
            s.with_bits()
        }
        None => name.to_string(),
    };

    let mut wf = load::full(path, &load::load_options())?;
    let sign;
    let handle = {
        let h = wf.hierarchy();
        sign = app.sign_args(h)?;
        let ctx = app.ctx(&sign);
        resolve::resolve_sig(h, &ctx, &spec)?
    };
    handle.load(&mut wf);
    let sig = handle.signal(&wf).ok_or_else(|| {
        anyhow::anyhow!("signal \"{}\" has no data in this file", handle.full_name)
    })?;

    let changes = sample::change_count(sig);
    let distinct = sample::distinct_values(sig);
    let first_tick = sample::first_change_tick(&wf, sig);
    let last_tick = sample::last_change_tick(&wf, sig);

    let clock_info = optional_clock(app, &mut wf);
    let h = wf.hierarchy();
    let ts = ts_of(h);

    let mut doc = Doc::new()
        .field(Field::s("file", path.display().to_string()))
        .field(Field::s("full_name", handle.full_name.clone()))
        .field(Field::s("name", handle.name.clone()))
        .field(Field::s(
            "scope",
            handle
                .full_name
                .rsplit_once('.')
                .map(|(s, _)| s.to_string())
                .unwrap_or_default(),
        ))
        .field(Field::u("width", handle.eff_width() as u64))
        .field(Field::s("var_type", handle.var_type))
        .field(Field::s("direction", handle.direction))
        .field(Field::s("encoding", encoding_text(h, handle.var)))
        .field(Field::opt_s(
            "index",
            handle.index.map(|(msb, lsb)| format!("{msb}:{lsb}")),
        ))
        .field(Field::s("signed", handle.signed.to_string()))
        .field(Field::s("signed_source", handle.signed_source.name()))
        .field(Field::s("radix", handle.radix.name()))
        .field(Field::u("changes", changes as u64))
        .field(Field::u("distinct_values", distinct as u64))
        .field(Field::u("transitions", changes.saturating_sub(1) as u64));

    // first/last change records
    for (key, tick) in [("first_change", first_tick), ("last_change", last_tick)] {
        let field = match tick {
            None => Field::text_value(key, "-", Value::Null),
            Some(tick) => {
                let cell = handle.cell_at_tick(&wf, tick);
                let cycle = clock_info.as_ref().map(|c| c.cycle_of(tick)).unwrap_or(-1);
                let text = format!(
                    "{} (tick {}, cycle {}, {})",
                    format_time(tick, ts),
                    tick,
                    cycle_str(cycle),
                    cell.text(handle.radix, handle.signed)
                );
                let json = json!({
                    "tick": tick,
                    "time": format_time(tick, ts),
                    "cycle": cycle_json(cycle),
                    "value": cell.val.as_ref().map(|v| v.render(handle.radix, handle.signed).unwrap_or_default()),
                    "bits": cell.val.as_ref().map(|v| v.json_bits()).unwrap_or(Value::Null),
                });
                Field::text_value(key, text, json)
            }
        };
        doc = doc.field(field);
    }
    Ok(doc)
}

pub fn clock(app: &App, list_only: bool) -> Result<Doc> {
    let path = app.require_file()?;
    let mut wf = load::full(path, &load::load_options())?;
    let info = match clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet)?
    {
        Some(info) => info,
        None => bail!("--clock none: no clock to report"),
    };
    let ts = ts_of(wf.hierarchy());

    let mut table = Table::named(
        "candidates",
        vec![
            "full_name".to_string(),
            "edges".to_string(),
            "median_period".to_string(),
        ],
    );
    for c in info.candidates.iter().take(5) {
        table.push(
            vec![
                c.full_name.clone(),
                c.edges.to_string(),
                c.median_period
                    .map(|p| format!("{} (tick {p})", format_time(p, ts)))
                    .unwrap_or_else(|| "-".to_string()),
            ],
            json!({
                "full_name": c.full_name,
                "edges": c.edges,
                "median_period": c.median_period,
                "median_period_time": c.median_period.map(|p| format_time(p, ts)),
            }),
        );
    }

    let mut doc = Doc::new();
    if list_only {
        return Ok(doc.table(table));
    }
    doc = doc
        .field(Field::s("file", path.display().to_string()))
        .field(Field::text_value(
            "clock",
            info.summary_text(ts),
            info.summary_json(ts),
        ))
        .field(Field::s("signal", info.full_name.clone()))
        .field(Field::s("edge", info.edge.name()))
        .field(Field::u("edges", info.edges.len() as u64))
        .field(Field::u("period_median", info.period_median.unwrap_or(0)))
        .field(Field::b("regular", info.regular()))
        .field(Field::s("detected_by", info.detected_by.name()));
    Ok(doc.table(table))
}

/// `signals --tree`: the scope structure with direct counts, useful for spotting
/// the `dut.*`/`ref.*` twins that `compare --pair-scopes` pairs up.
fn signals_tree(
    app: &App,
    h: &Hierarchy,
    matches: &[String],
    scope: Option<&str>,
    max_rows: Option<usize>,
) -> Result<Doc> {
    let sign = app.sign_args(h)?;
    let mut table = Table::new(vec![
        "name".to_string(),
        "kind".to_string(),
        "width".to_string(),
        "var_type".to_string(),
        "encoding".to_string(),
        "signed".to_string(),
    ]);
    let mut emitted = 0usize;
    let mut omitted = 0usize;
    let mut kept_signals = 0usize;

    #[allow(clippy::too_many_arguments)]
    fn walk(
        app: &App,
        h: &Hierarchy,
        sign: &crate::resolve::SignArgs,
        matches: &[String],
        table: &mut Table,
        emitted: &mut usize,
        omitted: &mut usize,
        kept_signals: &mut usize,
        max_rows: Option<usize>,
        depth: usize,
        name: &str,
        full_name: &str,
        scope_ref: Option<wellen::ScopeRef>,
    ) -> Result<()> {
        // prune scopes with no matching signal below them
        let any = match scope_ref {
            Some(s) => {
                let mut found = false;
                for v in h[s].all_vars(h) {
                    let name = resolve::var_full_name(h, v);
                    if matches.is_empty() || resolve::matches_any(matches, &name)? {
                        found = true;
                        break;
                    }
                }
                found
            }
            None => true,
        };
        if !any {
            return Ok(());
        }
        let emit_scope_line = scope_ref.is_some() || !name.is_empty();
        let indent = "  ".repeat(depth);
        let (vars, scopes) = match scope_ref {
            Some(s) => (h[s].vars(h).count(), h[s].scopes(h).count()),
            None => (h.vars().count(), h.scopes().count()),
        };
        if !emit_scope_line {
            // the implicit root is not a scope line
        } else if let Some(max) = max_rows
            && *emitted >= max
        {
            *omitted += 1;
        } else {
            *emitted += 1;
            table.push(
                vec![
                    format!("{indent}{name}"),
                    "scope".to_string(),
                    format!("{vars}v/{scopes}s"),
                    String::new(),
                    String::new(),
                    String::new(),
                ],
                json!({
                    "record": "scope",
                    "kind": "scope",
                    "full_name": full_name,
                    "name": name,
                    "depth": depth,
                    "vars": vars,
                    "scopes": scopes,
                }),
            );
        }
        // direct signals
        let vars_iter: Vec<wellen::VarRef> = match scope_ref {
            Some(s) => h[s].vars(h).collect(),
            None => h.vars().collect(),
        };
        for v in vars_iter {
            let vname = resolve::var_full_name(h, v);
            if !matches.is_empty() && !resolve::matches_any(matches, &vname)? {
                continue;
            }
            *kept_signals += 1;
            if let Some(max) = max_rows
                && *emitted >= max
            {
                *omitted += 1;
                continue;
            }
            *emitted += 1;
            let (signed, _) = resolve::signedness(h, v, &vname, &vname, sign, &app.types);
            let width = h[v].length(h);
            table.push(
                vec![
                    format!("{indent}  {}", h[v].name(h)),
                    "signal".to_string(),
                    width
                        .map(|w| w.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    var_type_name(h[v].var_type()).to_string(),
                    encoding_text(h, v),
                    signed.to_string(),
                ],
                json!({
                    "record": "signal",
                    "kind": "signal",
                    "full_name": vname,
                    "name": h[v].name(h),
                    "depth": depth + 1,
                    "width": width,
                    "var_type": var_type_name(h[v].var_type()),
                    "encoding": encoding_text(h, v),
                    "signed": signed,
                }),
            );
        }
        // child scopes
        let children: Vec<wellen::ScopeRef> = match scope_ref {
            Some(s) => h[s].scopes(h).collect(),
            None => h.scopes().collect(),
        };
        for child in children {
            let cname = h[child].full_name(h);
            let leaf = h[child].name(h).to_string();
            walk(
                app,
                h,
                sign,
                matches,
                table,
                emitted,
                omitted,
                kept_signals,
                max_rows,
                depth + 1,
                &leaf,
                &cname,
                Some(child),
            )?;
        }
        Ok(())
    }

    match scope {
        Some(prefix) => {
            let target = h
                .all_scopes()
                .find(|s| scope_full_name(h, *s) == prefix)
                .ok_or_else(|| anyhow::anyhow!("no scope \"{prefix}\" (see `waveq scopes`)"))?;
            walk(
                app,
                h,
                &sign,
                matches,
                &mut table,
                &mut emitted,
                &mut omitted,
                &mut kept_signals,
                max_rows,
                0,
                &scope_full_name(h, target),
                &scope_full_name(h, target),
                Some(target),
            )?;
        }
        None => {
            walk(
                app,
                h,
                &sign,
                matches,
                &mut table,
                &mut emitted,
                &mut omitted,
                &mut kept_signals,
                max_rows,
                0,
                "",
                "",
                None,
            )?;
        }
    }

    let mut doc = Doc::new()
        .field(Field::s("file", app.require_file()?.display().to_string()))
        .field(Field::u("signals", kept_signals as u64))
        .table(table);
    if omitted > 0 {
        app.note(&format!(
            "signals: truncated to {} tree lines ({omitted} omitted, use --max-rows 0 for all)",
            max_rows.unwrap_or(emitted)
        ));
        doc = doc
            .trailer_field(Field::b("truncated", true))
            .trailer_field(Field::u("omitted", omitted as u64));
    }
    Ok(doc)
}

pub fn config(app: &App) -> Result<Doc> {
    let mut effective = Table::named(
        "effective",
        vec![
            "section".to_string(),
            "key".to_string(),
            "value".to_string(),
        ],
    );
    let cfg_json = app.cfg.to_json();
    if let Some(obj) = cfg_json.as_object() {
        for (section, value) in obj {
            match value.as_object() {
                Some(map) => {
                    for (k, v) in map {
                        let text = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        effective.push(
                            vec![section.clone(), k.clone(), text],
                            json!({"section": section, "key": k, "value": v}),
                        );
                    }
                }
                None => {
                    effective.push(
                        vec![section.clone(), String::new(), value.to_string()],
                        json!({"section": section, "key": "", "value": value}),
                    );
                }
            }
        }
    }

    let mut doc = Doc::new()
        .field(Field::opt_s(
            "config",
            app.cfg_path.as_ref().map(|p| p.display().to_string()),
        ))
        .field(Field::opt_s(
            "file",
            app.file.as_ref().map(|p| p.display().to_string()),
        ))
        .table(effective);

    let Some(path) = app.file.clone() else {
        return Ok(doc.trailer_field(Field::text_value("resolved", "-", Value::Null)));
    };

    let hd = load::header(&path, &load::load_options())?;
    let h = &hd.hierarchy;
    let sign = app.sign_args(h)?;
    let ctx = app.ctx(&sign);

    let mut table = Table::named(
        "resolved",
        vec![
            "key".to_string(),
            "signal".to_string(),
            "width".to_string(),
            "signed".to_string(),
            "radix".to_string(),
            "source".to_string(),
        ],
    );
    let mut resolved: Vec<Value> = Vec::new();

    let mut resolve_one = |key: &str, spec: &str| -> Result<()> {
        let handle = resolve::resolve_sig(h, &ctx, spec)?;
        let record = json!({
            "key": key,
            "spec": spec,
            "full_name": handle.full_name,
            "width": handle.eff_width(),
            "signed": handle.signed,
            "signed_source": handle.signed_source.name(),
            "radix": handle.radix.name(),
            "var_type": handle.var_type,
            "direction": handle.direction,
        });
        table.push(
            vec![
                key.to_string(),
                handle.full_name.clone(),
                handle.eff_width().to_string(),
                handle.signed.to_string(),
                handle.radix.name().to_string(),
                handle.signed_source.name().to_string(),
            ],
            record.clone(),
        );
        resolved.push(record);
        Ok(())
    };

    // [types] entries: exact keys resolve directly, glob keys resolve every
    // matching signal
    for (key, _) in app.types.entries() {
        if key.contains('*') || key.contains('?') {
            let pattern = glob::Pattern::new(key)
                .map_err(|e| anyhow::anyhow!("invalid pattern \"{key}\" in [types]: {e}"))?;
            for v in resolve::iter_vars(h) {
                let full = resolve::var_full_name(h, v);
                if pattern.matches(&full) {
                    resolve_one(&format!("types.{key}={full}"), &full)?;
                }
            }
        } else {
            resolve_one(&format!("types.{key}"), key)?;
        }
    }
    if let Some(clock) = app.cfg.wave_clock() {
        resolve_one("wave.clock", clock)?;
    }
    if let Some(cmp) = &app.cfg.compare {
        for (key, spec) in [
            ("compare.in_valid", cmp.in_valid.as_deref()),
            ("compare.in_ready", cmp.in_ready.as_deref()),
            ("compare.out_valid", cmp.out_valid.as_deref()),
            ("compare.out_ready", cmp.out_ready.as_deref()),
            ("compare.gate", cmp.gate.as_deref()),
        ] {
            if let Some(spec) = spec {
                resolve_one(key, spec)?;
            }
        }
    }
    if let Some(groups) = &app.cfg.signals {
        for (group, members) in groups {
            for (i, member) in members.iter().enumerate() {
                resolve_one(&format!("signals.{group}[{i}]"), member)?;
            }
        }
    }
    if let Some(checks) = &app.cfg.checks {
        for (name, check) in checks {
            if let Some(a) = &check.a {
                resolve_one(&format!("checks.{name}.a"), a)?;
            }
            if let Some(b) = &check.b {
                resolve_one(&format!("checks.{name}.b"), b)?;
            }
        }
    }
    if let Some(queries) = &app.cfg.queries {
        for (name, query) in queries {
            for (i, member) in query.signals.iter().flatten().enumerate() {
                resolve_one(&format!("queries.{name}.signals[{i}]"), member)?;
            }
        }
    }

    let resolved = Value::Array(resolved);
    doc = doc.table(table).trailer_field(Field::text_value(
        "resolved",
        format!(
            "{} signals",
            resolved.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        resolved,
    ));
    Ok(doc)
}

/// Render a cell for text output (`-` when absent).
pub fn cell_text(cell: &Cell, radix: crate::format::Radix, signed: bool) -> String {
    cell.text(radix, signed)
}
