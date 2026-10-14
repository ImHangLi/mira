//! `mira show`: show a view in the TUI without a plugin, and `mira keep`: keep a shown view
//! as a view of a project plugin. A shown view is a finished one-off run whose result holds
//! the view data, so it appears in the asking agent's section of the TUI.

use std::io::Read;
use std::process::ExitCode;

use mira_protocol::config::PLUGIN_FILE;
use mira_protocol::error::{ErrorCode, ErrorInfo};
use mira_protocol::ids::{PluginId, RunId, ViewRef};
use mira_protocol::ipc::*;
use mira_protocol::limits::MAX_SHOWN_VIEW_BYTES;
use mira_protocol::manifest::ViewKind;
use mira_protocol::reply::ReplyContext;
use mira_protocol::run::RunRecord;
use mira_protocol::view::{Column, ColumnType, Row, SourceKind, TextFormat, ViewData};
use serde::Serialize;
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value, json};

use super::ctx::{Ctx, block_on};
use super::runtime::connect;
use super::save::{draft_with, entry_id, spaced};
use crate::output::invalid_argument;

pub struct ShowArgs {
    pub title: String,
    pub input: String,
    pub markdown: bool,
    pub task: Option<String>,
}

/// A JSON object that keeps its keys in the order they were written, so a table made from
/// objects has its columns in that order.
struct Ordered(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, Value>()? {
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// `[{"route": "/a", "p95": 120}, …]` → a table with one column per key, in the order the
/// keys first appear. A column is a number or boolean column when all its values are.
fn table_of(objects: Vec<Ordered>) -> Result<ViewData, ErrorInfo> {
    let mut keys: Vec<String> = Vec::new();
    for o in &objects {
        for (k, _) in &o.0 {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    if keys.is_empty() {
        return Err(invalid_argument("the objects have no fields to show"));
    }
    let types: Vec<ColumnType> = keys
        .iter()
        .map(|k| {
            let vals: Vec<&Value> = objects
                .iter()
                .filter_map(|o| o.0.iter().find(|(x, _)| x == k).map(|(_, v)| v))
                .filter(|v| !v.is_null())
                .collect();
            if !vals.is_empty() && vals.iter().all(|v| v.is_number()) {
                ColumnType::Number
            } else if !vals.is_empty() && vals.iter().all(|v| v.is_boolean()) {
                ColumnType::Boolean
            } else {
                ColumnType::Text
            }
        })
        .collect();
    let columns = keys
        .iter()
        .zip(&types)
        .map(|(k, t)| Column {
            id: k.clone(),
            label: k.clone(),
            column_type: *t,
        })
        .collect();
    let rows = objects
        .into_iter()
        .enumerate()
        .map(|(i, o)| {
            let mut values = Map::new();
            for (k, t) in keys.iter().zip(&types) {
                let v =
                    o.0.iter()
                        .find(|(x, _)| x == k)
                        .map_or(Value::Null, |(_, v)| v.clone());
                let v = match (t, v) {
                    (_, Value::Null) => Value::Null,
                    (ColumnType::Text, Value::String(s)) => Value::String(s),
                    (ColumnType::Text, other) => Value::String(other.to_string()),
                    (_, other) => other,
                };
                values.insert(k.clone(), v);
            }
            Row {
                id: (i + 1).to_string(),
                values,
            }
        })
        .collect();
    Ok(ViewData::Table { columns, rows })
}

fn read_all(input: &str) -> Result<Vec<u8>, ErrorInfo> {
    let mut b = Vec::new();
    let limit = (2 * MAX_SHOWN_VIEW_BYTES) as u64 + 1;
    if input == "-" {
        std::io::stdin()
            .take(limit)
            .read_to_end(&mut b)
            .map_err(|e| invalid_argument(e.to_string()))?;
    } else {
        std::fs::File::open(input)
            .and_then(|f| f.take(limit).read_to_end(&mut b))
            .map_err(|e| invalid_argument(format!("cannot read {input}: {e}")))?;
    }
    if b.len() as u64 >= limit {
        return Err(invalid_argument(
            "the input is larger than `mira show` takes (512 KiB)",
        ));
    }
    Ok(b)
}

/// The view data from the input: markdown text, a view object, or an array of objects.
fn data_of(bytes: &[u8], markdown: bool) -> Result<ViewData, ErrorInfo> {
    if markdown {
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| invalid_argument("--markdown text must be UTF-8"))?;
        return Ok(ViewData::Text {
            text,
            format: TextFormat::Markdown,
        });
    }
    let value = mira_protocol::strict_json::parse(bytes, 2 * MAX_SHOWN_VIEW_BYTES)
        .map_err(|e| ErrorInfo::new(ErrorCode::SCHEMA_INVALID, format!("invalid JSON: {e}")))?;
    match &value {
        Value::Object(o) if o.contains_key("kind") => serde_json::from_value(value)
            .map_err(|e| ErrorInfo::new(ErrorCode::SCHEMA_INVALID, format!("view data: {e}"))),
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let objects: Vec<Ordered> = serde_json::from_slice(bytes)
                .map_err(|e| ErrorInfo::new(ErrorCode::SCHEMA_INVALID, e.to_string()))?;
            table_of(objects)
        }
        _ => Err(invalid_argument(
            "give view data such as {\"kind\": \"table\", …}, a JSON array of objects, or text with --markdown",
        )),
    }
}

pub fn show(ctx: &Ctx, a: ShowArgs) -> ExitCode {
    block_on(async {
        let data = match read_all(&a.input).and_then(|b| data_of(&b, a.markdown)) {
            Ok(d) => d,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let issues = data.validate("/data");
        if !issues.0.is_empty() {
            return ctx.fail(ReplyContext::default(), issues.to_error_info());
        }
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let p = ViewShowParams {
            title: a.title,
            data,
            requester: crate::requester::detect(a.task),
        };
        match client.call::<_, InvokeAccepted>(Method::ViewShow, &p).await {
            Ok(r) => ctx.emit(&r, |d| {
                format!(
                    "shown in `mira` as {}\nkeep it as a view: mira keep {}",
                    d.run_id, d.run_id
                )
            }),
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}

/// A view definition, with its fields in the order people read them.
#[derive(Serialize)]
struct ViewDef<'a> {
    id: &'a str,
    title: &'a str,
    description: &'a str,
    kind: ViewKind,
    persistence: &'static str,
}

/// The reply data of `mira keep`.
#[derive(Serialize)]
struct Kept {
    view_ref: ViewRef,
    plugin_file: String,
    view_revision: mira_protocol::ids::ViewRevision,
}

pub struct KeepArgs {
    pub run: String,
    pub id: Option<String>,
    pub title: Option<String>,
    pub plugin: String,
}

pub fn keep(ctx: &Ctx, a: KeepArgs) -> ExitCode {
    block_on(async {
        let parsed = (|| {
            let run_id = RunId::parse(a.run.clone())
                .map_err(|e| invalid_argument(format!("{e}: `{}`", a.run)))?;
            let plugin = a
                .plugin
                .parse::<PluginId>()
                .map_err(|e| invalid_argument(format!("--plugin: {e}")))?;
            Ok::<_, ErrorInfo>((run_id, plugin, ctx.paths()?))
        })();
        let (run_id, plugin, paths) = match parsed {
            Ok(v) => v,
            Err(e) => return ctx.fail(ReplyContext::default(), e),
        };
        let mut client = match connect(ctx).await {
            Ok(c) => c,
            Err(code) => return code,
        };
        let rec = match client
            .call::<_, RunRecord>(Method::RunGet, &RunGetParams { run_id })
            .await
        {
            Ok(r) => match r.into_data() {
                Ok(d) => d,
                Err(e) => return ctx.fail(client.context(), e),
            },
            Err(e) => return ctx.fail(client.context(), e.to_error_info()),
        };
        let shown = (|| {
            let kind = rec.view_kind.ok_or_else(|| {
                invalid_argument(format!(
                    "{} is not a shown view; `mira keep` takes the run ID that `mira show` printed",
                    rec.run_id
                ))
            })?;
            let data: ViewData = rec
                .result
                .as_ref()
                .and_then(|r| serde_json::from_value(r.data.clone()).ok())
                .ok_or_else(|| {
                    ErrorInfo::new(ErrorCode::INTERNAL, "the shown view has no readable data")
                })?;
            let title = a.title.clone().unwrap_or_else(|| rec.label.clone());
            let id = entry_id(a.id.as_deref(), &title)?;
            let by = rec
                .requester
                .as_ref()
                .filter(|r| r.id != "you")
                .map_or(String::new(), |r| format!(" by {}", r.name));
            let description = format!(
                "Shown{by} with `mira show` and kept. Update it with `mira publish {plugin}.{id}`."
            );
            let def = spaced(&ViewDef {
                id: &id,
                title: &title,
                description: &description,
                kind,
                persistence: "last",
            })?;
            let draft = draft_with(&paths, &plugin, "views", &id, &def)?;
            let view_ref: ViewRef = format!("{plugin}.{id}")
                .parse()
                .map_err(|e| ErrorInfo::new(ErrorCode::INTERNAL, format!("{e}")))?;
            Ok::<_, ErrorInfo>((draft, view_ref, id, data))
        })();
        let (draft, view_ref, id, data) = match shown {
            Ok(v) => v,
            Err(e) => return ctx.fail(client.context(), e),
        };
        match draft.apply(ctx).await {
            Ok(reply) => {
                if let Some(e) = reply.error() {
                    return ctx.fail(reply.context(), e.clone());
                }
            }
            Err((c, e)) => return ctx.fail(c, e),
        }
        let frame = json!({"api": 1, "type": "view", "view_id": id, "op": "replace", "data": data});
        let Value::Object(frame) = frame else {
            return ctx.fail(
                client.context(),
                ErrorInfo::new(ErrorCode::INTERNAL, "view frame"),
            );
        };
        let p = ViewPublishParams {
            view_ref: view_ref.clone(),
            frame,
            source_kind: SourceKind::Cli,
            expected_view_revision: None,
            request_key: None,
        };
        match client
            .call::<_, PublishResult>(Method::ViewPublish, &p)
            .await
        {
            Ok(r) => {
                let reply = r.map(|d| Kept {
                    view_ref,
                    plugin_file: format!(".mira/{}/{PLUGIN_FILE}", draft.own),
                    view_revision: d.view_revision,
                });
                ctx.emit(&reply, |k| {
                    format!(
                        "kept `{}` in {}\nread it: mira view {}   or select it in `mira`",
                        k.view_ref, k.plugin_file, k.view_ref
                    )
                })
            }
            Err(e) => ctx.fail(client.context(), e.to_error_info()),
        }
    })
}
