//! `email-mcp --describe`: print the server instructions and every tool's
//! description and parameters, so what the agent sees can be reviewed by a human.

use std::fmt::Write;

use rmcp::ServerHandler;
use serde_json::{Map, Value, json};

use crate::server::EmailServer;

const WIDTH: usize = 100;

pub fn json(server: &EmailServer) -> Value {
    let info = server.get_info();
    json!({
        "server": info.server_info,
        "instructions": info.instructions,
        "tools": server.tools(),
    })
}

pub fn text(server: &EmailServer) -> String {
    let info = server.get_info();
    let mut out = String::new();
    let _ = writeln!(out, "# {} {}\n", info.server_info.name, info.server_info.version);
    if let Some(i) = &info.instructions {
        out += &wrap(i, "", WIDTH);
        out += "\n";
    }
    for tool in server.tools() {
        let _ = writeln!(out, "\n## {}\n", tool.name);
        if let Some(d) = &tool.description {
            out += &wrap(d, "", WIDTH);
        }
        let schema = &*tool.input_schema;
        let defs = schema.get("$defs").and_then(Value::as_object);
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(props) = schema.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty()) else {
            out += "\n  (no parameters)\n";
            continue;
        };
        out += "\n";
        for (name, prop) in props {
            let (ty, choices) = type_of(prop, defs);
            let mut line = format!("  {name}: {ty}");
            if required.contains(&name.as_str()) {
                line += " (required)";
            } else if let Some(d) = prop.get("default") {
                let _ = write!(line, " = {d}");
            }
            out += &line;
            out += "\n";
            if let Some(d) = description(prop, defs) {
                out += &wrap(&d, "      ", WIDTH);
            }
            for (value, desc) in choices {
                out += &wrap(&format!("{value} — {desc}"), "      • ", WIDTH);
            }
        }
    }
    out
}

/// A short type name, plus the choices if it is an enum.
fn type_of(prop: &Value, defs: Option<&Map<String, Value>>) -> (String, Vec<(String, String)>) {
    let prop = resolve(prop, defs);
    if let Some(alts) = prop.get("anyOf").and_then(Value::as_array) {
        // Option<T> is rendered as anyOf [T, null].
        let mut nullable = false;
        let mut found = None;
        for alt in alts {
            if alt.get("type").and_then(Value::as_str) == Some("null") {
                nullable = true;
            } else {
                found.get_or_insert_with(|| type_of(alt, defs));
            }
        }
        let (ty, choices) = found.unwrap_or_else(|| ("any".into(), vec![]));
        return (if nullable { format!("{ty}?") } else { ty }, choices);
    }
    if let Some(alts) = prop.get("oneOf").and_then(Value::as_array) {
        let choices: Vec<_> = alts
            .iter()
            .filter_map(|a| {
                let v = a.get("const")?.as_str()?.to_owned();
                Some((v, a.get("description").and_then(Value::as_str).unwrap_or("").to_owned()))
            })
            .collect();
        let names: Vec<_> = choices.iter().map(|(v, _)| v.as_str()).collect();
        return (names.join(" | "), choices);
    }
    let ty = match prop.get("type") {
        Some(Value::String(t)) => base_type(t, prop, defs),
        Some(Value::Array(ts)) => {
            let non_null: Vec<_> = ts.iter().filter_map(Value::as_str).filter(|t| *t != "null").collect();
            let ty = non_null.iter().map(|t| base_type(t, prop, defs)).collect::<Vec<_>>().join(" | ");
            if non_null.len() < ts.len() { format!("{ty}?") } else { ty }
        }
        _ => "any".into(),
    };
    (ty, vec![])
}

fn base_type(t: &str, prop: &Value, defs: Option<&Map<String, Value>>) -> String {
    match (t, prop.get("items")) {
        ("array", Some(items)) => format!("[{}]", type_of(items, defs).0),
        _ => t.to_owned(),
    }
}

/// Follow a local `$ref` into `$defs`.
fn resolve<'a>(prop: &'a Value, defs: Option<&'a Map<String, Value>>) -> &'a Value {
    prop.get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .and_then(|name| defs?.get(name))
        .unwrap_or(prop)
}

/// The property's own description, or failing that, that of the type it refers to.
fn description(prop: &Value, defs: Option<&Map<String, Value>>) -> Option<String> {
    let own = |v: &Value| v.get("description").and_then(Value::as_str).map(str::to_owned);
    own(prop).or_else(|| {
        let alts = prop.get("anyOf").and_then(Value::as_array)?;
        alts.iter().find_map(|a| own(resolve(a, defs)))
    })
}

/// Word-wrap `text`; the first line starts with `indent`, later lines with as many spaces.
fn wrap(text: &str, indent: &str, width: usize) -> String {
    let hang = " ".repeat(indent.chars().count());
    let mut out = String::new();
    let mut line = indent.to_owned();
    for (i, para) in text.split('\n').enumerate() {
        if i > 0 {
            line = hang.clone();
        }
        let mut empty = true;
        for word in para.split_whitespace() {
            if !empty && line.chars().count() + 1 + word.chars().count() > width {
                out += &line;
                out += "\n";
                line = hang.clone();
                empty = true;
            }
            if !empty {
                line.push(' ');
            }
            line += word;
            empty = false;
        }
        out += line.trim_end();
        out += "\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn server() -> EmailServer {
        EmailServer::new(Arc::new(crate::app::App::default()))
    }

    #[test]
    fn lists_every_tool() {
        let s = server();
        let text = text(&s);
        for t in s.tools() {
            assert!(text.contains(&format!("## {}\n", t.name)), "missing {}", t.name);
        }
    }

    #[test]
    fn renders_enums_and_optionals() {
        let text = text(&server());
        let update = text.split("## update_account\n").nth(1).unwrap();
        let update = update.split("\n## ").next().unwrap();
        assert!(update.contains("remove_by: move_to_trash | delete?"), "{update}");
        assert!(update.contains("• delete — Delete messages permanently"), "{update}");
        assert!(update.contains("account: string (required)"), "{update}");
        assert!(update.contains("trash_mailbox: string?"), "{update}");
        assert!(text.contains("limit: integer = 20"));
        assert!(text.contains("uids: [integer] (required)"));
    }
}
