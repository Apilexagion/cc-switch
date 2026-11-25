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
