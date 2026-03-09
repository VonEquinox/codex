use std::collections::HashSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use toml_edit::ArrayOfTables;
use toml_edit::DocumentMut;
use toml_edit::Item as TomlItem;
use toml_edit::Table as TomlTable;

pub(crate) const DEFAULT_HOOK_EVENT_KEYS: &[&str] = &[
    "session_start",
    "session_end",
    "user_prompt_submit",
    "pre_tool_use",
    "permission_request",
    "notification",
    "post_tool_use",
    "post_tool_use_failure",
    "stop",
    "teammate_idle",
    "task_completed",
    "config_change",
    "subagent_start",
    "subagent_stop",
    "pre_compact",
    "worktree_create",
    "worktree_remove",
];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct HookId {
    pub(crate) event: String,
    pub(crate) index: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct HookListEntry {
    pub(crate) id: HookId,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) selected_description: Option<String>,
    pub(crate) search_value: String,
}

#[derive(Clone, Debug)]
pub(crate) enum HookMutation {
    Add { event: String, snippet: String },
    Replace { id: HookId, snippet: String },
    Delete { id: HookId },
    ReplaceRaw { contents: String },
}

pub(crate) fn list_hook_entries(contents: &str) -> Result<Vec<HookListEntry>> {
    let doc = parse_document(contents).context("parse config.toml")?;
    let Some(hooks_table) = hooks_table_for_read(&doc)? else {
        return Ok(Vec::new());
    };

    let ordered_event_keys = ordered_event_keys(hooks_table);
    let mut entries = Vec::new();

    for event_key in ordered_event_keys {
        let Some(aot) = hooks_table
            .get(&event_key)
            .and_then(TomlItem::as_array_of_tables)
        else {
            continue;
        };

        for (index, table) in aot.iter().enumerate() {
            entries.push(hook_entry_summary(&event_key, index, table)?);
        }
    }

    Ok(entries)
}

pub(crate) fn template_for_event(event: &str) -> String {
    format!(
        "# Define exactly one hook entry.\n\
         # See docs/hooks.md for supported fields and matcher rules.\n\
         \n\
         [[hooks.{event}]]\n\
         name = \"\"\n\
         command = \"\"\n\
         \n\
         [hooks.{event}.matcher]\n\
         # matcher = \"*\"\n"
    )
}

pub(crate) fn apply_mutation(contents: &str, mutation: HookMutation) -> Result<String> {
    match mutation {
        HookMutation::ReplaceRaw { contents } => {
            let cleaned = cleaned_raw_contents(&contents);
            parse_document(&cleaned).context("config.toml is not valid TOML")?;
            Ok(cleaned)
        }
        HookMutation::Add { event, snippet } => {
            let mut doc = parse_document(contents).context("parse config.toml")?;
            let entry = parse_single_entry_snippet(&snippet, &event)?;
            ensure_event_array_for_write(&mut doc, &event)?.push(entry);
            Ok(doc.to_string())
        }
        HookMutation::Replace { id, snippet } => {
            let mut doc = parse_document(contents).context("parse config.toml")?;
            let entry = parse_single_entry_snippet(&snippet, &id.event)?;
            let array = ensure_event_array_for_write(&mut doc, &id.event)?;
            let Some(existing) = array.get_mut(id.index) else {
                return Err(anyhow!(
                    "hook index out of range: {}[{}]",
                    id.event,
                    id.index
                ));
            };
            *existing = entry;
            Ok(doc.to_string())
        }
        HookMutation::Delete { id } => {
            let mut doc = parse_document(contents).context("parse config.toml")?;
            let hooks_table = ensure_hooks_table_for_write(&mut doc)?;
            let array = hooks_table
                .get_mut(id.event.as_str())
                .and_then(TomlItem::as_array_of_tables_mut)
                .ok_or_else(|| anyhow!("hooks.{} is not an array-of-tables", id.event))?;
            if id.index >= array.len() {
                return Err(anyhow!(
                    "hook index out of range: {}[{}]",
                    id.event,
                    id.index
                ));
            }
            array.remove(id.index);
            if array.is_empty() {
                hooks_table.remove(id.event.as_str());
            }
            Ok(doc.to_string())
        }
    }
}

pub(crate) fn editor_seed_for_entry(event: &str, index: usize, contents: &str) -> Result<String> {
    let doc = parse_document(contents).context("parse config.toml")?;
    let hooks_table =
        hooks_table_for_read(&doc)?.ok_or_else(|| anyhow!("missing [hooks] table"))?;
    let aot = hooks_table
        .get(event)
        .and_then(TomlItem::as_array_of_tables)
        .ok_or_else(|| anyhow!("hooks.{event} is not an array-of-tables"))?;
    let table = aot
        .get(index)
        .ok_or_else(|| anyhow!("hook index out of range: {event}[{index}]"))?;
    Ok(single_entry_doc(event, table).to_string())
}

fn parse_document(contents: &str) -> Result<DocumentMut> {
    if contents.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    contents.parse::<DocumentMut>().map_err(Into::into)
}

fn hooks_table_for_read(doc: &DocumentMut) -> Result<Option<&TomlTable>> {
    match doc.get("hooks") {
        None => Ok(None),
        Some(item) => item
            .as_table()
            .ok_or_else(|| anyhow!("[hooks] is not a table"))
            .map(Some),
    }
}

fn ordered_event_keys(hooks_table: &TomlTable) -> Vec<String> {
    let mut known = HashSet::new();
    let mut out = DEFAULT_HOOK_EVENT_KEYS
        .iter()
        .map(|key| {
            known.insert(*key);
            (*key).to_string()
        })
        .collect::<Vec<_>>();

    for (key, item) in hooks_table.iter() {
        if !item.is_array_of_tables() {
            continue;
        }
        if known.contains(key) {
            continue;
        }
        out.push(key.to_string());
    }

    out
}

fn hook_entry_summary(event: &str, index: usize, table: &TomlTable) -> Result<HookListEntry> {
    let name = table
        .get("name")
        .and_then(TomlItem::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    let command_value = table
        .get("command")
        .map(TomlItem::to_string)
        .unwrap_or_else(|| "<missing command>".to_string());
    let matcher = table.get("matcher").and_then(TomlItem::as_table);
    let matcher_summary = matcher.and_then(matcher_summary);

    let title = match name.as_deref() {
        Some(name) => format!("{event}: {name}"),
        None => format!("{event}: {command_value}"),
    };

    let mut selected = format!("Event: {event}\nIndex: {index}\nCommand: {command_value}");
    if let Some(name) = name.as_deref() {
        selected.push_str(&format!("\nName: {name}"));
    }
    if let Some(matcher) = matcher_summary.as_deref() {
        selected.push_str(&format!("\nMatcher: {matcher}"));
    }

    let search_value = format!("{event} {index} {title} {command_value}").to_lowercase();

    Ok(HookListEntry {
        id: HookId {
            event: event.to_string(),
            index,
        },
        title,
        description: Some(command_value),
        selected_description: Some(selected),
        search_value,
    })
}

fn matcher_summary(table: &TomlTable) -> Option<String> {
    for key in ["matcher", "tool_name", "tool_name_regex", "prompt_regex"] {
        let value = table.get(key)?;
        let rendered = value.to_string();
        if rendered.trim().is_empty() {
            continue;
        }
        return Some(format!("{key}={rendered}"));
    }
    None
}

fn single_entry_doc(event: &str, table: &TomlTable) -> DocumentMut {
    let mut doc = DocumentMut::new();
    let root = doc.as_table_mut();
    let mut hooks = TomlTable::new();
    hooks.set_implicit(false);

    let mut aot = ArrayOfTables::new();
    aot.push(table.clone());
    hooks.insert(event, TomlItem::ArrayOfTables(aot));

    root.insert("hooks", TomlItem::Table(hooks));
    doc
}

fn parse_single_entry_snippet(snippet: &str, event: &str) -> Result<TomlTable> {
    let doc = parse_document(snippet).context("parse snippet")?;
    let hooks_table = hooks_table_for_read(&doc)?.ok_or_else(|| {
        anyhow!("snippet must define exactly one [[hooks.{event}]] entry (missing [hooks])")
    })?;
    let aot = hooks_table
        .get(event)
        .and_then(TomlItem::as_array_of_tables)
        .ok_or_else(|| {
            anyhow!(
                "snippet must define exactly one [[hooks.{event}]] entry (missing hooks.{event})"
            )
        })?;
    if aot.len() != 1 {
        return Err(anyhow!(
            "snippet must define exactly one [[hooks.{event}]] entry (found {})",
            aot.len()
        ));
    }
    aot.get(0)
        .cloned()
        .ok_or_else(|| anyhow!("missing hooks.{event}[0]"))
}

fn ensure_hooks_table_for_write(doc: &mut DocumentMut) -> Result<&mut TomlTable> {
    let root = doc.as_table_mut();
    if !root.contains_key("hooks") {
        let mut hooks = TomlTable::new();
        hooks.set_implicit(false);
        root.insert("hooks", TomlItem::Table(hooks));
    }

    root.get_mut("hooks")
        .and_then(TomlItem::as_table_mut)
        .ok_or_else(|| anyhow!("[hooks] is not a table"))
}

fn ensure_event_array_for_write<'a>(
    doc: &'a mut DocumentMut,
    event: &str,
) -> Result<&'a mut ArrayOfTables> {
    let hooks_table = ensure_hooks_table_for_write(doc)?;
    if !hooks_table.contains_key(event) {
        hooks_table.insert(event, TomlItem::ArrayOfTables(ArrayOfTables::new()));
    }

    hooks_table
        .get_mut(event)
        .and_then(TomlItem::as_array_of_tables_mut)
        .ok_or_else(|| anyhow!("hooks.{event} is not an array-of-tables"))
}

fn cleaned_raw_contents(contents: &str) -> String {
    let cleaned = contents.trim_end();
    if cleaned.is_empty() {
        return String::new();
    }
    format!("{cleaned}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use toml::Value as TomlValue;

    fn toml_value(contents: &str) -> TomlValue {
        toml::from_str(contents).expect("valid toml")
    }

    fn get_command(value: &TomlValue, event: &str, index: usize) -> TomlValue {
        value
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(|arr| arr.as_array())
            .and_then(|arr| arr.get(index))
            .and_then(|entry| entry.get("command"))
            .cloned()
            .expect("command")
    }

    #[test]
    fn mutation_add_creates_event_array() {
        let base = "";
        let out = apply_mutation(
            base,
            HookMutation::Add {
                event: "stop".to_string(),
                snippet: "[[hooks.stop]]\ncommand = \"echo hi\"\n".to_string(),
            },
        )
        .expect("apply");
        let parsed = toml_value(&out);
        assert_eq!(get_command(&parsed, "stop", 0).as_str(), Some("echo hi"));
    }

    #[test]
    fn mutation_replace_updates_entry() {
        let base = r#"
[hooks]

[[hooks.stop]]
command = "echo a"

[[hooks.stop]]
command = "echo b"
"#;
        let out = apply_mutation(
            base,
            HookMutation::Replace {
                id: HookId {
                    event: "stop".to_string(),
                    index: 0,
                },
                snippet: "[[hooks.stop]]\ncommand = \"echo z\"\n".to_string(),
            },
        )
        .expect("apply");
        let parsed = toml_value(&out);
        assert_eq!(get_command(&parsed, "stop", 0).as_str(), Some("echo z"));
        assert_eq!(get_command(&parsed, "stop", 1).as_str(), Some("echo b"));
    }

    #[test]
    fn mutation_delete_drops_key_when_last_removed() {
        let base = r#"
[hooks]

[[hooks.stop]]
command = "echo a"
"#;
        let out = apply_mutation(
            base,
            HookMutation::Delete {
                id: HookId {
                    event: "stop".to_string(),
                    index: 0,
                },
            },
        )
        .expect("apply");
        let parsed = toml_value(&out);
        let hooks = parsed
            .get("hooks")
            .and_then(|v| v.as_table())
            .expect("hooks");
        assert!(hooks.get("stop").is_none(), "expected hooks.stop removed");
    }

    #[test]
    fn list_includes_unknown_hook_events() {
        let base = r#"
[hooks]

[[hooks.future_event]]
command = "echo hi"
"#;
        let entries = list_hook_entries(base).expect("list");
        assert!(
            entries.iter().any(|entry| entry.id.event == "future_event"),
            "expected unknown event to appear"
        );
    }
}
