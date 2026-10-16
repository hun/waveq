//! Output: `text`, `json` and `jsonl` rendering of a uniform document model.
//!
//! Every command produces a [`Doc`]: ordered header fields, zero or more
//! tables of rows, and trailing fields. Text renders a table with aligned
//! columns, `json` one document, `jsonl` one object per row (with a leading
//! `{"record":"header",...}` line when the command has a header block).

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Text,
    Json,
    Jsonl,
}

/// One `key: value` output field.
#[derive(Debug, Clone)]
pub struct Field {
    pub key: String,
    pub text: String,
    pub json: Value,
}

impl Field {
    pub fn text_value(key: &str, text: impl Into<String>, json: Value) -> Field {
        Field {
            key: key.to_string(),
            text: text.into(),
            json,
        }
    }

    pub fn s(key: &str, value: impl Into<String>) -> Field {
        let s: String = value.into();
        Field {
            key: key.to_string(),
            text: s.clone(),
            json: Value::String(s),
        }
    }

    pub fn opt_s(key: &str, value: Option<impl Into<String>>) -> Field {
        match value {
            Some(v) => Field::s(key, v),
            None => Field {
                key: key.to_string(),
                text: "-".to_string(),
                json: Value::Null,
            },
        }
    }

    pub fn n(key: &str, value: i64) -> Field {
        Field {
            key: key.to_string(),
            text: value.to_string(),
            json: value.into(),
        }
    }

    pub fn u(key: &str, value: u64) -> Field {
        Field {
            key: key.to_string(),
            text: value.to_string(),
            json: value.into(),
        }
    }

    pub fn b(key: &str, value: bool) -> Field {
        Field {
            key: key.to_string(),
            text: value.to_string(),
            json: value.into(),
        }
    }

    pub fn opt_n(key: &str, value: Option<i64>) -> Field {
        match value {
            Some(v) => Field::n(key, v),
            None => Field {
                key: key.to_string(),
                text: "-".to_string(),
                json: Value::Null,
            },
        }
    }
}

/// One row of a table: aligned text cells plus the JSON record.
#[derive(Debug, Clone)]
pub struct Row {
    pub text: Vec<String>,
    pub json: Value,
}

impl Row {
    pub fn new(text: Vec<String>, json: Value) -> Row {
        Row { text, json }
    }
}

#[derive(Debug, Clone)]
pub struct Table {
    /// Empty for the primary table (which serializes as `"rows"`).
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
}

impl Table {
    pub fn new(columns: Vec<String>) -> Table {
        Table {
            name: String::new(),
            columns,
            rows: Vec::new(),
        }
    }

    pub fn named(name: &str, columns: Vec<String>) -> Table {
        Table {
            name: name.to_string(),
            columns,
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, text: Vec<String>, json: Value) {
        self.rows.push(Row::new(text, json));
    }
}

/// A complete command result.
#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub fields: Vec<Field>,
    pub tables: Vec<Table>,
    pub trailer: Vec<Field>,
    /// Whether `jsonl` emits a leading header record.
    pub header_record: bool,
    /// Process exit code contributed by this document (0 or 1).
    pub exit: u8,
}

impl Doc {
    pub fn new() -> Doc {
        Doc::default()
    }

    pub fn field(mut self, f: Field) -> Doc {
        self.fields.push(f);
        self
    }

    pub fn trailer_field(mut self, f: Field) -> Doc {
        self.trailer.push(f);
        self
    }

    pub fn table(mut self, t: Table) -> Doc {
        self.tables.push(t);
        self
    }

    pub fn header_record(mut self, value: bool) -> Doc {
        self.header_record = value;
        self
    }

    pub fn exit(mut self, value: u8) -> Doc {
        self.exit = value;
        self
    }

    fn fields_json(fields: &[Field]) -> Map<String, Value> {
        let mut m = Map::new();
        for f in fields {
            m.insert(f.key.clone(), f.json.clone());
        }
        m
    }

    pub fn to_json(&self) -> Value {
        let mut m = Self::fields_json(&self.fields);
        for (i, t) in self.tables.iter().enumerate() {
            let key = if t.name.is_empty() {
                "rows".to_string()
            } else {
                t.name.clone()
            };
            if i == 0 || !t.name.is_empty() {
                m.insert(
                    key,
                    Value::Array(t.rows.iter().map(|r| r.json.clone()).collect()),
                );
            }
        }
        for f in &self.trailer {
            m.insert(f.key.clone(), f.json.clone());
        }
        Value::Object(m)
    }

    fn text(&self) -> String {
        let mut out = String::new();
        for f in &self.fields {
            out.push_str(&format!("{}: {}\n", f.key, f.text));
        }
        for (i, t) in self.tables.iter().enumerate() {
            if t.rows.is_empty() {
                continue;
            }
            if i > 0 && !t.name.is_empty() {
                out.push_str(&format!("{}:\n", t.name));
            }
            out.push_str(&crate::format::format_table(&t.columns, &t.row_texts()));
        }
        for f in &self.trailer {
            out.push_str(&format!("{}: {}\n", f.key, f.text));
        }
        out
    }

    pub fn render(&self, format: Format) -> String {
        match format {
            Format::Text => self.text(),
            Format::Json => {
                let mut s = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
                s.push('\n');
                s
            }
            Format::Jsonl => {
                let mut out = String::new();
                let header = Self::fields_json(&self.fields);
                let emit = |value: &Value, out: &mut String| {
                    out.push_str(&serde_json::to_string(value).unwrap_or_default());
                    out.push('\n');
                };
                if self.header_record && !header.is_empty() {
                    let mut h = header.clone();
                    let mut rec = Map::new();
                    rec.insert("record".into(), json!("header"));
                    for (k, v) in h.iter() {
                        rec.insert(k.clone(), v.clone());
                    }
                    h = rec;
                    emit(&Value::Object(h), &mut out);
                }
                for t in &self.tables {
                    let record = if t.name.is_empty() {
                        "row"
                    } else {
                        t.name.as_str()
                    };
                    for r in &t.rows {
                        match r.json.as_object() {
                            Some(obj) => {
                                let mut rec = Map::new();
                                rec.insert("record".into(), json!(record));
                                if !self.header_record {
                                    for (k, v) in header.iter() {
                                        rec.insert(k.clone(), v.clone());
                                    }
                                }
                                for (k, v) in obj.iter() {
                                    rec.insert(k.clone(), v.clone());
                                }
                                emit(&Value::Object(rec), &mut out);
                            }
                            None => emit(&r.json, &mut out),
                        }
                    }
                }
                if !self.trailer.is_empty() {
                    let mut rec = Map::new();
                    rec.insert("record".into(), json!("footer"));
                    for (k, v) in Self::fields_json(&self.trailer).iter() {
                        rec.insert(k.clone(), v.clone());
                    }
                    emit(&Value::Object(rec), &mut out);
                }
                out
            }
        }
    }
}

impl Table {
    fn row_texts(&self) -> Vec<Vec<String>> {
        self.rows.iter().map(|r| r.text.clone()).collect()
    }
}

pub fn print(doc: &Doc, format: Format) {
    print!("{}", doc.render(format));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Doc {
        let mut t = Table::new(vec!["cycle".into(), "sum".into()]);
        t.push(
            vec!["13".into(), "45".into()],
            json!({"cycle": 13, "sum": 45}),
        );
        Doc::new()
            .field(Field::s("file", "dump.vcd"))
            .table(t)
            .trailer_field(Field::n("mismatches", 1))
            .header_record(true)
    }

    #[test]
    fn text_layout() {
        let out = sample().render(Format::Text);
        assert_eq!(out, "file: dump.vcd\ncycle sum\n13    45\nmismatches: 1\n");
    }

    #[test]
    fn json_shape() {
        let v: Value = serde_json::from_str(&sample().render(Format::Json)).unwrap();
        assert_eq!(v["file"], "dump.vcd");
        assert_eq!(v["rows"][0]["cycle"], 13);
        assert_eq!(v["mismatches"], 1);
    }

    #[test]
    fn jsonl_named_table_records() {
        let mut main = Table::new(vec!["cycle".into()]);
        main.push(vec!["1".into()], json!({"cycle": 1}));
        let mut pairs = Table::named("pairs", vec!["pair".into()]);
        pairs.push(vec!["0".into()], json!({"pair": 0}));
        let out = Doc::new().table(main).table(pairs).render(Format::Jsonl);
        assert_eq!(
            out,
            "{\"record\":\"row\",\"cycle\":1}\n{\"record\":\"pairs\",\"pair\":0}\n"
        );
    }

    #[test]
    fn jsonl_records() {
        let out = sample().render(Format::Jsonl);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with(r#"{"record":"header","file":"dump.vcd"}"#));
        assert!(lines[1].starts_with(r#"{"record":"row","cycle":13"#));
        assert!(lines[2].starts_with(r#"{"record":"footer","mismatches":1}"#));
    }

    #[test]
    fn jsonl_merges_fields_without_header_record() {
        let mut t = Table::new(vec!["signal".into()]);
        t.push(vec!["tb.a".into()], json!({"signal": "tb.a"}));
        let doc = Doc::new()
            .field(Field::n("tick", 5))
            .field(Field::s("time", "5ns"))
            .table(t);
        let out = doc.render(Format::Jsonl);
        assert_eq!(
            out,
            "{\"record\":\"row\",\"tick\":5,\"time\":\"5ns\",\"signal\":\"tb.a\"}\n"
        );
    }
}
