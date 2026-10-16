//! Value-reading commands: `value`, `changes`, `table`.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use wellen::Timescale;

use crate::format::{format_time, parse_time};
use crate::output::{Doc, Field, Table};
use crate::resolve::SigHandle;
use crate::sample::{Cell, Val};
use crate::{clock, load};

use super::{App, cycle_json, cycle_str, fmt_time};

/// Fully resolved clock; cycle-indexed output is impossible without one.
pub fn require_clock(app: &App, wf: &mut wellen::simple::Waveform) -> Result<clock::ClockInfo> {
    match clock::resolve_clock(wf, app.clock_spec.as_deref(), app.edge, app.quiet)? {
        Some(info) => Ok(info),
        None => bail!("this command needs cycles: do not pass --clock none here"),
    }
}

fn parse_cycle_range(s: &str, what: &str) -> Result<(i64, i64)> {
    let (a, b) = s
        .split_once("..")
        .ok_or_else(|| anyhow::anyhow!("{what} expects A..B, got \"{s}\""))?;
    let a: i64 = a
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{what}: invalid start in \"{s}\""))?;
    let b: i64 = b
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{what}: invalid end in \"{s}\""))?;
    Ok((a, b))
}

fn parse_time_range(s: &str, ts: Option<Timescale>) -> Result<(u64, u64)> {
    let (a, b) = s
        .split_once("..")
        .ok_or_else(|| anyhow::anyhow!("--time expects A..B, got \"{s}\""))?;
    Ok((parse_time(a, ts)?, parse_time(b, ts)?))
}

fn apply_slice(val: &Val, handle: &SigHandle) -> Val {
    match val {
        Val::Bits(b) => match handle.apply_slice(b) {
            Ok(sliced) => Val::Bits(sliced),
            Err(_) => Val::Bits(b.clone()),
        },
        other => other.clone(),
    }
}

pub(crate) fn signal_value_json(cell: &Cell, handle: &SigHandle) -> Value {
    let mut m = Map::new();
    m.insert(
        "value".into(),
        cell.val
            .as_ref()
            .map(|v| v.render(handle.radix, handle.signed).unwrap_or_default())
            .into(),
    );
    m.insert(
        "bits".into(),
        cell.val
            .as_ref()
            .map(|v| v.json_bits())
            .unwrap_or(Value::Null),
    );
    m.insert(
        "has_x".into(),
        cell.val.as_ref().is_some_and(|v| v.has_xz()).into(),
    );
    Value::Object(m)
}

pub fn value(app: &App, at: Option<&str>, cycle: Option<i64>, signals: &[String]) -> Result<Doc> {
    let path = app.require_file()?;
    if at.is_some() && cycle.is_some() {
        bail!("--at and --cycle are mutually exclusive");
    }
    if at.is_none() && cycle.is_none() {
        bail!("specify --at <TIME> or --cycle <N>");
    }
    let specs = app.expand_signals(signals)?;

    let mut wf = load::full(path, &load::load_options())?;
    let sign;
    let handles = {
        let h = wf.hierarchy();
        sign = app.sign_args(h)?;
        app.resolve_all(h, &sign, &specs)?
    };
    for handle in &handles {
        handle.load(&mut wf);
    }
    let ts = wf.hierarchy().timescale();

    let (tick, explicit_cycle) = match (at, cycle) {
        (Some(spec), None) => (parse_time(spec, ts)?, None),
        (None, Some(c)) => {
            let info = require_clock(app, &mut wf)?;
            let origin = super::resolve_anchor(app, &mut wf, &info)?;
            let last = info.last_cycle() - origin;
            let local = c + origin;
            let tick = info.tick_of(local).ok_or_else(|| {
                anyhow::anyhow!(
                    "cycle {c} is out of range for clock \"{}\" (0..{})",
                    info.full_name,
                    last
                )
            })?;
            (tick, Some(c))
        }
        _ => unreachable!(),
    };

    let cycle_index = match explicit_cycle {
        Some(c) => c,
        None => {
            match clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet) {
                Ok(Some(info)) => {
                    let origin = super::resolve_anchor(app, &mut wf, &info)?;
                    info.cycle_of(tick) - origin
                }
                _ => {
                    super::guard_single_dump(app)?;
                    -1
                }
            }
        }
    };

    let mut table = Table::new(vec![
        "signal".to_string(),
        "cycle".to_string(),
        "tick".to_string(),
        "time".to_string(),
        "value".to_string(),
        "bits".to_string(),
    ]);
    for handle in &handles {
        let cell = handle.cell_at_tick(&wf, tick);
        let mut row = Map::new();
        row.insert("signal".into(), handle.full_name.clone().into());
        row.insert("width".into(), handle.eff_width().into());
        row.insert("signed".into(), handle.signed.into());
        row.insert("radix".into(), handle.radix.name().into());
        row.insert("value".into(), signal_value_json(&cell, handle));
        row.insert(
            "change_tick".into(),
            cell.change_tick.map(Value::from).unwrap_or(Value::Null),
        );
        table.push(
            vec![
                handle.full_name.clone(),
                cycle_str(cycle_index),
                tick.to_string(),
                format_time(tick, ts),
                cell.text(handle.radix, handle.signed),
                cell.bits().unwrap_or("-").to_string(),
            ],
            Value::Object(row),
        );
    }

    Ok(Doc::new()
        .field(Field::s("file", path.display().to_string()))
        .field(Field::u("tick", tick))
        .field(Field::s("time", format_time(tick, ts)))
        .field(Field::text_value(
            "cycle",
            cycle_str(cycle_index),
            cycle_json(cycle_index),
        ))
        .table(table))
}

#[allow(clippy::too_many_arguments)]
pub fn changes(
    app: &App,
    signals: &[String],
    from_cycle: Option<i64>,
    to_cycle: Option<i64>,
    from: Option<&str>,
    to: Option<&str>,
    delta: bool,
    no_initial: bool,
    at_edges_only: bool,
) -> Result<Doc> {
    let path = app.require_file()?;
    let specs = app.expand_signals(signals)?;

    let mut wf = load::full(path, &load::load_options())?;
    let sign;
    let handles = {
        let h = wf.hierarchy();
        sign = app.sign_args(h)?;
        app.resolve_all(h, &sign, &specs)?
    };
    for handle in &handles {
        handle.load(&mut wf);
    }
    let ts = wf.hierarchy().timescale();

    let clock_info = clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet)?;
    let origin = match &clock_info {
        Some(info) => super::resolve_anchor(app, &mut wf, info)?,
        None => {
            super::guard_single_dump(app)?;
            0
        }
    };
    if clock_info.is_none() {
        app.note("changes: --clock none: printing ticks/times only (no cycle column)");
    }

    let from_tick = match from {
        Some(s) => Some(parse_time(s, ts)?),
        None => None,
    };
    let to_tick = match to {
        Some(s) => Some(parse_time(s, ts)?),
        None => None,
    };
    if let (Some(a), Some(b)) = (from_tick, to_tick)
        && a > b
    {
        bail!("--from {a} is after --to {b}");
    }
    let all_ticks: Vec<u64> = if at_edges_only {
        clock_info
            .as_ref()
            .map(|c| c.edges.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    struct ChangeRow {
        tick: u64,
        order: usize,
        signal: String,
        cell: Cell,
        cycle: i64,
    }

    let mut rows: Vec<ChangeRow> = Vec::new();
    let mut total = 0usize;
    for (order, handle) in handles.iter().enumerate() {
        let Some(sig) = handle.signal(&wf) else {
            continue;
        };
        let mut is_first = true;
        for (idx, value) in sig.iter_changes() {
            let tick = wf.time_table()[idx as usize];
            let initial = std::mem::take(&mut is_first);
            total += 1;
            if no_initial && initial {
                continue;
            }
            let cycle = clock_info
                .as_ref()
                .map(|c| c.cycle_of(tick) - origin)
                .unwrap_or(-1);
            if let Some(a) = from_cycle
                && cycle < a
            {
                continue;
            }
            if let Some(b) = to_cycle
                && cycle > b
            {
                continue;
            }
            if let Some(a) = from_tick
                && tick < a
            {
                continue;
            }
            if let Some(b) = to_tick
                && tick > b
            {
                continue;
            }
            if at_edges_only && all_ticks.binary_search(&tick).is_err() {
                continue;
            }
            let val = apply_slice(&Val::from_ref(value), handle);
            rows.push(ChangeRow {
                tick,
                order,
                signal: handle.full_name.clone(),
                cell: Cell {
                    val: Some(val),
                    change_tick: Some(tick),
                },
                cycle,
            });
        }
    }
    rows.sort_by(|a, b| a.tick.cmp(&b.tick).then(a.order.cmp(&b.order)));

    let mut columns = vec![
        if clock_info.is_some() {
            "cycle"
        } else {
            "tick"
        }
        .to_string(),
        "time".to_string(),
        "signal".to_string(),
        "value".to_string(),
        "bits".to_string(),
    ];
    if delta {
        columns.push("delta_cycles".to_string());
    }

    let max_rows = app.max_rows(50);
    let mut table = Table::new(columns);
    let mut last_cycle_of_signal: Vec<Option<i64>> = vec![None; handles.len()];
    let mut omitted = 0usize;
    for (i, row) in rows.iter().enumerate() {
        if let Some(max) = max_rows
            && table.rows.len() >= max
        {
            omitted = rows.len() - i;
            break;
        }
        let handle = &handles[row.order];
        let d = match last_cycle_of_signal[row.order] {
            Some(prev) if row.cycle >= 0 && prev >= 0 => Some(row.cycle - prev),
            _ => None,
        };
        last_cycle_of_signal[row.order] = Some(row.cycle);
        let mut json_row = Map::new();
        json_row.insert("cycle".into(), cycle_json(row.cycle));
        json_row.insert("tick".into(), row.tick.into());
        json_row.insert("time".into(), format_time(row.tick, ts).into());
        json_row.insert("signal".into(), row.signal.clone().into());
        json_row.insert("width".into(), handle.eff_width().into());
        json_row.insert("radix".into(), handle.radix.name().into());
        json_row.insert("signed".into(), handle.signed.into());
        json_row.insert(
            "value".into(),
            row.cell
                .val
                .as_ref()
                .map(|v| v.render(handle.radix, handle.signed).unwrap_or_default())
                .into(),
        );
        json_row.insert(
            "bits".into(),
            row.cell
                .val
                .as_ref()
                .map(|v| v.json_bits())
                .unwrap_or(Value::Null),
        );
        json_row.insert(
            "has_x".into(),
            row.cell.val.as_ref().is_some_and(|v| v.has_xz()).into(),
        );
        json_row.insert("pre_clock".into(), (row.cycle < 0).into());
        if delta {
            json_row.insert(
                "delta_cycles".into(),
                d.map(Value::from).unwrap_or(Value::Null),
            );
        }
        let mut text = vec![
            if clock_info.is_some() {
                cycle_str(row.cycle)
            } else {
                row.tick.to_string()
            },
            format_time(row.tick, ts),
            row.signal.clone(),
            row.cell.text(handle.radix, handle.signed),
            row.cell.bits().unwrap_or("-").to_string(),
        ];
        if delta {
            text.push(d.map(|d| d.to_string()).unwrap_or_else(|| "-".to_string()));
        }
        table.push(text, Value::Object(json_row));
    }

    let mut doc = Doc::new()
        .field(Field::s("file", path.display().to_string()))
        .field(Field::u("total_changes", total as u64))
        .field(Field::u("changes", rows.len() as u64))
        .table(table);
    if omitted > 0 {
        app.note(&format!(
            "changes: showing the first {} of {} changes (use --max-rows 0 for all)",
            max_rows.unwrap_or(rows.len()),
            rows.len()
        ));
        doc = doc
            .trailer_field(Field::b("truncated", true))
            .trailer_field(Field::u("omitted", omitted as u64));
    }
    Ok(doc)
}

#[allow(clippy::too_many_arguments)]
pub fn table(
    app: &App,
    signals: &[String],
    cycles: Option<&str>,
    from_cycle: Option<i64>,
    to_cycle: Option<i64>,
    at_cycle: Option<i64>,
    context: Option<i64>,
    time: Option<&str>,
    query: Option<&str>,
) -> Result<Doc> {
    let path = app.require_file()?;

    // a named query provides defaults; CLI arguments override them
    let mut signals = signals.to_vec();
    let mut cycles = cycles.map(|s| s.to_string());
    let mut from_cycle = from_cycle;
    let mut to_cycle = to_cycle;
    let mut at_cycle = at_cycle;
    let mut context = context;
    let mut time = time.map(|s| s.to_string());
    if let Some(name) = query {
        let Some(q) = app.cfg.query(name) else {
            let names = app.cfg.query_names();
            if names.is_empty() {
                bail!("no query \"{name}\": the config defines no [queries]");
            }
            bail!("no query \"{name}\"; queries: {}", names.join(", "));
        };
        if signals.is_empty() {
            signals = q.signals.clone().unwrap_or_default();
        }
        if cycles.is_none() {
            cycles = q.cycles.clone();
        }
        if from_cycle.is_none() {
            from_cycle = q.from_cycle;
        }
        if to_cycle.is_none() {
            to_cycle = q.to_cycle;
        }
        if at_cycle.is_none() {
            at_cycle = q.at_cycle;
        }
        if context.is_none() {
            context = q.context;
        }
    }
    if signals.is_empty() {
        bail!("no signals: pass --signal <NAME> (or --query <NAME>)");
    }
    let specs = app.expand_signals(&signals)?;

    let mut wf = load::full(path, &load::load_options())?;
    let sign;
    let handles = {
        let h = wf.hierarchy();
        sign = app.sign_args(h)?;
        app.resolve_all(h, &sign, &specs)?
    };
    for handle in &handles {
        handle.load(&mut wf);
    }
    let ts = wf.hierarchy().timescale();
    let clock_info = clock::resolve_clock(&mut wf, app.clock_spec.as_deref(), app.edge, app.quiet)?;
    let origin = match &clock_info {
        Some(info) => super::resolve_anchor(app, &mut wf, info)?,
        None => {
            super::guard_single_dump(app)?;
            0
        }
    };

    // `--time` bounds become cycle bounds when a clock is available
    if let (Some(info), Some(range)) = (clock_info.as_ref(), time.as_deref()) {
        if cycles.is_some() {
            bail!("--time and --cycles are mutually exclusive");
        }
        let (a, b) = parse_time_range(range, ts)?;
        let (ca, cb) = (info.cycle_of(a) - origin, info.cycle_of(b) - origin);
        if ca < 0 || cb < 0 {
            bail!(
                "cycle range starts before the first clock edge (first edge at {} (tick {})){}",
                fmt_time(ts, info.edges[0]),
                info.edges[0],
                if origin > 0 {
                    "; --anchor moved cycle 0 later than that"
                } else {
                    ""
                }
            );
        }
        cycles = Some(format!("{ca}..{cb}"));
        time = None;
    }

    let max_rows = app.max_rows(50);

    match clock_info {
        Some(info) => {
            let last_logical = info.last_cycle() - origin;
            let (mut a, mut b) = match (&cycles, at_cycle) {
                (Some(range), _) => parse_cycle_range(range, "--cycles")?,
                (None, Some(c)) => {
                    let k = context.unwrap_or(0).max(0);
                    (c - k, c + k)
                }
                (None, None) => (0, last_logical),
            };
            if let Some(f) = from_cycle {
                a = a.max(f);
            }
            if let Some(t) = to_cycle {
                b = b.min(t);
            }
            // cycles below this bound do not exist in the file: local cycle
            // `origin` is logical 0, so logical cycles start at `-origin`
            let first_logical = 0.max(-origin);
            if a < first_logical {
                bail!(
                    "cycle range starts before the first clock edge (local cycle {origin} at {} (tick {})); \
                     the first cycle here is {first_logical}",
                    fmt_time(ts, info.edges[origin.max(0) as usize]),
                    info.edges[origin.max(0) as usize]
                );
            }
            if b < a {
                bail!("empty cycle range {a}..{b}");
            }
            b = b.min(last_logical);
            let mut columns = vec!["cycle".to_string(), "time".to_string(), "tick".to_string()];
            for handle in &handles {
                columns.push(handle.full_name.clone());
            }
            let mut table = Table::new(columns);
            let mut omitted = 0usize;
            let mut written = 0i64;
            for c in a..=b {
                if let Some(max) = max_rows
                    && table.rows.len() >= max
                {
                    omitted = (b - c + 1) as usize;
                    break;
                }
                written += 1;
                let tick = info.edges[(c + origin) as usize];
                let mut values = Map::new();
                let mut text = vec![c.to_string(), format_time(tick, ts), tick.to_string()];
                for handle in &handles {
                    let cell = handle.cell_at_cycle(&wf, &info.edges, c + origin);
                    values.insert(handle.full_name.clone(), signal_value_json(&cell, handle));
                    text.push(cell.text(handle.radix, handle.signed));
                }
                table.push(
                    text,
                    json!({
                        "cycle": c,
                        "tick": tick,
                        "time": format_time(tick, ts),
                        "values": Value::Object(values),
                    }),
                );
            }
            let mut doc = Doc::new()
                .field(Field::s("file", path.display().to_string()))
                .field(Field::s("clock", info.full_name.clone()))
                .field(Field::n("from_cycle", a))
                .field(Field::n("to_cycle", a + written - 1))
                .table(table);
            if omitted > 0 {
                app.note(&format!(
                    "table: truncated to {} rows of {} cycles (use --max-rows 0 for all)",
                    max_rows.unwrap_or((b - a + 1) as usize),
                    (b - a + 1)
                ));
                doc = doc
                    .trailer_field(Field::b("truncated", true))
                    .trailer_field(Field::u("omitted", omitted as u64));
            }
            Ok(doc)
        }
        None => {
            // tick-domain table: one row per time-table entry
            let mut columns = vec!["tick".to_string(), "time".to_string()];
            for handle in &handles {
                columns.push(handle.full_name.clone());
            }
            let mut table = Table::new(columns);
            let (from_tick, to_tick) = match time.as_deref() {
                Some(range) => parse_time_range(range, ts)?,
                None => (0, u64::MAX),
            };
            let ticks: Vec<u64> = wf
                .time_table()
                .iter()
                .copied()
                .filter(|t| *t >= from_tick && *t <= to_tick)
                .collect();
            let mut omitted = 0usize;
            for (i, tick) in ticks.iter().enumerate() {
                if let Some(max) = max_rows
                    && table.rows.len() >= max
                {
                    omitted = ticks.len() - i;
                    break;
                }
                let mut values = Map::new();
                let mut text = vec![tick.to_string(), format_time(*tick, ts)];
                for handle in &handles {
                    let cell = handle.cell_at_tick(&wf, *tick);
                    values.insert(handle.full_name.clone(), signal_value_json(&cell, handle));
                    text.push(cell.text(handle.radix, handle.signed));
                }
                table.push(
                    text,
                    json!({
                        "tick": tick,
                        "time": format_time(*tick, ts),
                        "values": Value::Object(values),
                    }),
                );
            }
            app.note("table: --clock none: one row per time-table tick");
            let mut doc = Doc::new()
                .field(Field::s("file", path.display().to_string()))
                .field(Field::text_value("clock", "-", Value::Null))
                .table(table);
            if omitted > 0 {
                doc = doc
                    .trailer_field(Field::b("truncated", true))
                    .trailer_field(Field::u("omitted", omitted as u64));
            }
            Ok(doc)
        }
    }
}
