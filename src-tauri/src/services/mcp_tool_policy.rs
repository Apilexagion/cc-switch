//! Device policy is not stuffed into the shared MCP spec (which would leak
//! client-specific options to every other agent). The native client enforces it.
use crate::{error::AppError, live::engine::DeviceStore, sync_utils::MutexExt};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::Mutex,
};
type Rules = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;
static WRITES: Mutex<()> = Mutex::new(());
fn file() -> std::path::PathBuf {
    DeviceStore::for_device().file("mcp-tool-policy.json")
}
fn load() -> Result<Rules, AppError> {
    match fs::read(file()) {
        Ok(v) => serde_json::from_slice(&v)
            .map_err(|_| AppError::Message("Invalid MCP tool policy".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Rules::new()),
        Err(e) => Err(AppError::io(file(), e)),
    }
}
fn save(r: &Rules) -> Result<(), AppError> {
    crate::config::atomic_write_private(
        &file(),
        &serde_json::to_vec_pretty(r)
            .map_err(|_| AppError::Message("Cannot serialize tool policy".into()))?,
    )
}
pub fn listed(app: &str, id: &str) -> Result<BTreeSet<String>, AppError> {
    Ok(load()?
        .get(app)
        .and_then(|m| m.get(id))
        .cloned()
        .unwrap_or_default())
}
pub fn field(app: &str) -> Result<&'static str, AppError> {
    match app {
        "codex" => Ok("disabled_tools"),
        "gemini" => Ok("excludeTools"),
        _ => Err(AppError::Message(
            "This client has no managed native tool filter".into(),
        )),
    }
}
fn merge(spec: &Value, k: &str, names: &BTreeSet<String>) -> Result<Value, AppError> {
    if names.is_empty() {
        return Ok(spec.clone());
    }
    let mut copy = spec.clone();
    let obj = copy
        .as_object_mut()
        .ok_or_else(|| AppError::Message("Invalid MCP spec".into()))?;
    let arr = obj
        .entry(k)
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| AppError::Message("Invalid native tool filter".into()))?;
    for n in names {
        if !arr.iter().any(|v| v.as_str() == Some(n)) {
            arr.push(n.clone().into());
        }
    }
    Ok(copy)
}
pub(crate) fn effective(app: &str, id: &str, spec: &Value) -> Result<Value, AppError> {
    merge(spec, field(app)?, &listed(app, id)?)
}
pub fn change(
    app: &str,
    id: &str,
    name: &str,
    blocked: bool,
    apply: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let _ = field(app)?;
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(AppError::Message("Invalid MCP tool name".into()));
    }
    let _lock = WRITES.lock_recover();
    let old = load()?;
    let mut new = old.clone();
    let set = new
        .entry(app.into())
        .or_default()
        .entry(id.into())
        .or_default();
    if blocked {
        set.insert(name.into());
    } else {
        set.remove(name);
    }
    save(&new)?;
    if let Err(err) = apply() {
        save(&old)?;
        return Err(err);
    }
    Ok(())
}
// Claude Code enforces per-tool deny rules in its own settings, not in the
// server spec. Record ownership in the same guarded write as settings.json so
// a user-created deny can never be claimed/removed by the UI.
fn claude_rule_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 128
        && !part.contains("__")
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
type ClaudeSettings = (crate::live::engine::LiveFile, Option<Vec<u8>>, Vec<Value>);
fn read_claude() -> Result<ClaudeSettings, AppError> {
    let f = crate::services::provider::claude_direct::settings_file();
    let pre = crate::live::engine::read_current(&f.path)?;
    let (doc, _) = crate::live::patch::json::parse(&f.path, pre.as_deref())?;
    let deny = match doc.pointer("/permissions/deny") {
        Some(v) => v
            .as_array()
            .cloned()
            .ok_or_else(|| AppError::Message("Invalid Claude permission list".into()))?,
        None => vec![],
    };
    Ok((f, pre, deny))
}
pub fn claude_view(id: &str) -> Result<(Vec<String>, Vec<String>, bool), AppError> {
    if !claude_rule_part(id) {
        return Err(AppError::Message(
            "Unsupported Claude MCP server name".into(),
        ));
    }
    let (_, _, deny) = read_claude()?;
    let owned = listed("claude", id)?;
    let prefix = format!("mcp__{id}__");
    let mut ours = vec![];
    let mut manual = vec![];
    let mut all = false;
    for rule in deny.iter().filter_map(Value::as_str) {
        if rule == format!("mcp__{id}") || rule == format!("{prefix}*") || rule == "mcp__*" {
            all = true;
        }
        if let Some(n) = rule.strip_prefix(&prefix).filter(|n| claude_rule_part(n)) {
            if owned.contains(n) {
                ours.push(n.into());
            } else {
                manual.push(n.into());
            }
        }
    }
    Ok((ours, manual, all))
}
pub fn change_claude(
    db: &crate::database::Database,
    id: &str,
    name: &str,
    blocked: bool,
) -> Result<(), AppError> {
    use crate::live::{
        engine::{digest, read_current, LiveFile},
        patch::{json::JsonPatch, Guarded, KeyPath, LivePatch, WholeFile},
    };
    use crate::mode::{
        operation::{AppWrite, FileChange},
        state::{op, PendingTarget},
    };
    if !claude_rule_part(id) || !claude_rule_part(name) {
        return Err(AppError::Message("Unsupported Claude MCP rule name".into()));
    }
    if !crate::config::get_claude_config_dir().is_dir() {
        return Err(AppError::Message("Initialize Claude Code first".into()));
    }
    let _guard = WRITES.lock_recover();
    let write = AppWrite::begin(db, "claude")?;
    let policy = LiveFile::private(file());
    let ppre = read_current(&policy.path)?;
    let mut rules: Rules = match ppre.as_deref() {
        Some(b) => serde_json::from_slice(b)
            .map_err(|_| AppError::Message("Invalid MCP tool policy".into()))?,
        None => Default::default(),
    };
    let managed = rules
        .entry("claude".into())
        .or_default()
        .entry(id.into())
        .or_default();
    let (f, pre, mut deny) = read_claude()?;
    let entry = Value::String(format!("mcp__{id}__{name}"));
    if blocked {
        if deny.contains(&entry) {
            return Ok(());
        }
        deny.push(entry);
        managed.insert(name.into());
    } else {
        if !managed.remove(name) {
            return Err(AppError::Message(
                "This Claude deny rule is not owned by CC Switch".into(),
            ));
        }
        deny.retain(|v| v != &entry);
    }
    let json = JsonPatch {
        set: vec![(KeyPath::new(&["permissions", "deny"]), Value::Array(deny))],
        ..Default::default()
    };
    let cfg = Guarded {
        expected_pre: digest(pre.as_deref()),
        then: WholeFile::Write(json.apply(&f.path, pre.as_deref())?),
    };
    let own = Guarded {
        expected_pre: digest(ppre.as_deref()),
        then: WholeFile::Write(
            serde_json::to_vec_pretty(&rules).map_err(|e| AppError::Message(e.to_string()))?,
        ),
    };
    write.run(
        op::APPLY,
        &[
            FileChange {
                file: f,
                patch: &cfg,
            },
            FileChange {
                file: policy,
                patch: &own,
            },
        ],
        PendingTarget::default(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_explicit_user_denials() {
        let src = serde_json::json!({"disabled_tools":["mine"]});
        let out = merge(&src, "disabled_tools", &["other".into()].into()).unwrap();
        assert_eq!(out["disabled_tools"], serde_json::json!(["mine", "other"]));
        assert_eq!(src["disabled_tools"], serde_json::json!(["mine"]));
    }
}
