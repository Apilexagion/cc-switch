//! Read-only preview of the managed configuration (never auth files, keychain,
//! token refresh, recover/settle, live writes or remote catalog discovery).
use super::*;
use crate::live::engine::LiveFile;
use crate::live::patch::LivePatch;
use crate::live::project::grok::GrokConfigPatch;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
type Fields = BTreeMap<String, Value>;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    file: String,
    field: String,
    current: &'static str,
    exit: &'static str,
}
#[derive(Serialize)]
pub struct Preview {
    pub token: String,
    pub rows: Vec<Row>,
}
static SALT: LazyLock<uuid::Uuid> = LazyLock::new(uuid::Uuid::new_v4);

fn fields(name: &str, bytes: &[u8]) -> Result<Fields, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "Configuration is not UTF-8")?
        .trim_start_matches('\u{feff}');
    if name == ".env" {
        return Ok(crate::live::patch::dotenv::entries(text)
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect());
    }
    let v = if name.ends_with(".toml") {
        serde_json::to_value(
            text.parse::<toml::Table>()
                .map_err(|_| "Invalid TOML configuration")?,
        )
        .map_err(|_| "Invalid TOML value")?
    } else if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(text).map_err(|_| "Invalid JSON configuration")?
    };
    let mut out = Fields::new();
    for (k, v) in v.as_object().ok_or("Configuration is not an object")? {
        if k == "env" && v.is_object() {
            for (key, value) in v.as_object().unwrap() {
                out.insert(format!("env.{key}"), value.clone());
            }
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(out)
}
fn action(key: &str, cur: &Fields, choices: &[Fields]) -> &'static str {
    let a = cur.get(key);
    let mut answers = choices.iter().map(|m| match (a, m.get(key)) {
        (a, b) if a == b => "keep",
        (_, None) => "remove",
        (None, _) => "add",
        _ => "change",
    });
    let first = answers.next().unwrap_or("conditional");
    if answers.all(|s| s == first) {
        first
    } else {
        "conditional"
    }
}
#[derive(Default)]
struct Builder {
    rows: Vec<Row>,
    fingerprint: Sha256,
}
impl Builder {
    fn file(
        &mut self,
        file: LiveFile,
        expected: &[&dyn LivePatch],
        exit: &[&dyn LivePatch],
    ) -> Result<(), String> {
        let name = file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("Missing filename")?;
        let before = crate::live::engine::read_current(&file.path)
            .map_err(|_| format!("Cannot read {name}"))?;
        self.fingerprint
            .update(file.path.to_string_lossy().as_bytes());
        self.fingerprint
            .update(crate::live::engine::digest(before.as_deref()).unwrap_or_default());
        let cur = fields(name, before.as_deref().unwrap_or_default())?;
        let mut project = |patches: &[&dyn LivePatch]| -> Result<Vec<Fields>, String> {
            patches
                .iter()
                .map(|p| {
                    let value = p
                        .apply(&file.path, before.as_deref())
                        .map_err(|_| format!("Cannot inspect {name}"))?;
                    self.fingerprint.update(Sha256::digest(&value));
                    fields(name, &value)
                })
                .collect()
        };
        let expected = project(expected)?;
        let exit = project(exit)?;
        let keys: BTreeSet<_> = cur
            .keys()
            .chain(expected.iter().chain(&exit).flat_map(|m| m.keys()))
            .collect();
        for key in keys {
            let current = action(key, &cur, &expected);
            let after = action(key, &cur, &exit);
            if current != "keep" || after != "keep" {
                self.rows.push(Row {
                    file: name.into(),
                    field: key.clone(),
                    current,
                    exit: after,
                });
            }
        }
        Ok(())
    }
}
fn read_only_mode(app: &AppType) -> Result<ModeState, String> {
    let store = DeviceStore::for_device();
    if crate::mode::state::pending(&store, app.as_str())
        .map_err(err)?
        .is_some()
    {
        return Err("Configuration change pending; try again after it finishes".into());
    }
    crate::mode::state::mode_state(&store, app.as_str()).map_err(err)
}
async fn locked(state: &AppState, app: &AppType) -> Result<Preview, String> {
    let mode = read_only_mode(app)?;
    if !mode.is_proxy() || !mode.attached {
        return Err("This client is not attached to the gateway".into());
    }
    let route = route_provider(state, app, &mode)?.ok_or("Routing provider is unavailable")?;
    let direct = direct_provider(state, app)?;
    let direct = usable_direct(app, direct.as_ref());
    let live = LiveNow::of(state, app, &mode)?;
    let (url, cx_url) = state.proxy_service.build_proxy_urls().await?;
    let mut b = Builder::default();
    b.fingerprint.update(SALT.as_bytes());
    b.fingerprint.update(app.as_str());
    b.fingerprint.update(route.id.as_bytes());
    if let Some(d) = direct {
        b.fingerprint.update(d.id.as_bytes());
    }
    match app {
        AppType::Claude => {
            let members = published_members(state, app, &settled_stack(app)?, &route)?;
            let others = members
                .iter()
                .filter(|m| !m.route)
                .map(|m| &m.provider)
                .collect::<Vec<_>>();
            let (expected, _) = claude_contract(
                &route,
                &url,
                &stack::claude_published(&members),
                stack::claude_route_default(&members).as_ref(),
                &others,
            );
            let project = |target: &ClaudeProjection, op| {
                let mut p = direct_patch(live.claude_exclusive_owner().as_ref(), target);
                crate::services::provider::claude_official::strip_from_patch(&mut p);
                claude_direct::for_live(op, &p)
            };
            let target = direct
                .map(|d| ClaudeProjection::of(&d.settings_config))
                .unwrap_or_default();
            b.file(
                claude_direct::settings_file(),
                &[&project(&expected, op::ATTACH)],
                &[&project(&target, op::EXIT)],
            )?;
        }
        AppType::Gemini => {
            let expected = GeminiProjection::proxy_contract(
                &GeminiProjection::of(&route.settings_config, false),
                &url,
                PROXY_TOKEN_PLACEHOLDER,
            );
            let target = direct_or_empty(
                app,
                direct,
                gemini_direct::projection,
                GeminiProjection::empty,
            );
            b.file(
                gemini_direct::env_file(),
                &[&expected.env_patch()],
                &[&target.env_patch()],
            )?;
            b.file(
                gemini_direct::settings_file(),
                &[&expected.settings_patch()],
                &[&target.settings_patch()],
            )?;
        }
        AppType::GrokBuild => {
            let expected = GrokProjection::proxy_contract(
                &grok_direct::projection(&route).map_err(|_| "Invalid routed model")?,
                &format!("{}/grokbuild/v1", url.trim_end_matches('/')),
                PROXY_TOKEN_PLACEHOLDER,
            )
            .map_err(|_| "Invalid Grok projection")?;
            let target = direct_or_empty(app, direct, grok_direct::projection, || GrokProjection {
                table: None,
            });
            let retired =
                grok_direct::retired_tables(&DeviceStore::for_device(), None).map_err(err)?;
            let patch = |p| GrokConfigPatch::direct(p, retired.clone(), PROXY_TOKEN_PLACEHOLDER);
            b.file(
                grok_direct::config_file(),
                &[&patch(&expected)],
                &[&patch(&target)],
            )?;
        }
        AppType::Codex => {
            let members = published_members(state, app, &settled_stack(app)?, &route)?;
            let owner = live.codex_owner();
            let proxy_target = codex_direct::Target::Proxy {
                route: &route,
                base_url: &cx_url,
                stack: &members,
            };
            let direct_target = codex_direct::Target::Direct(direct);
            let opts = |t| {
                codex_direct::inspect_configs(&state.db, &owner, t)
                    .map_err(|_| "Cannot project Codex configuration without changing it")
            };
            let current = opts(&proxy_target)?;
            let next = opts(&direct_target)?;
            let current: Vec<&dyn LivePatch> =
                current.iter().map(|p| p as &dyn LivePatch).collect();
            let next: Vec<&dyn LivePatch> = next.iter().map(|p| p as &dyn LivePatch).collect();
            b.file(
                LiveFile::private(crate::codex_config::get_codex_config_path()),
                &current,
                &next,
            )?;
        }
        _ => return Err("Configuration inspection is not supported for this client".into()),
    }
    Ok(Preview {
        token: format!("{:x}", b.fingerprint.finalize()),
        rows: b.rows,
    })
}
pub async fn preview(state: &AppState, app: &AppType) -> Result<Preview, String> {
    let _lock = state.proxy_service.lock_switch_for_app(app.as_str()).await;
    locked(state, app).await
}
pub async fn apply(state: &AppState, app: &AppType, token: &str, exit: bool) -> Result<(), String> {
    {
        let _lock = state.proxy_service.lock_switch_for_app(app.as_str()).await;
        if locked(state, app).await?.token != token {
            return Err("The preview changed; inspect again before applying".into());
        }
        if exit {
            exit_locked(state, app, false)?;
        } else {
            let mode = read_only_mode(app)?;
            let route =
                route_provider(state, app, &mode)?.ok_or("Routing provider is unavailable")?;
            let live = LiveNow::of(state, app, &mode)?;
            write_proxy(state, app, op::ATTACH, &route, &live, mode, None).await?;
            sync_resources(state, app);
        }
    }
    if exit {
        let _ = state.proxy_service.clear_provider_health_for_app(app).await;
        stop_server_if_unused(state).await;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compares_values_but_never_returns_them_and_distinguishes_conditional_routes() {
        let a = fields(
            "settings.json",
            br#"{"other":false,"env":{"KEY":"secret"}}"#,
        )
        .unwrap();
        let b = fields(
            "settings.json",
            br#"{"other":false,"env":{"KEY":"another"}}"#,
        )
        .unwrap();
        assert_eq!(
            action("env.KEY", &a, &[b.clone(), a.clone()]),
            "conditional"
        );
        assert_eq!(action("env.KEY", &a, &[Fields::new()]), "remove");
        assert_eq!(action("other", &a, &[b]), "keep");
    }
}
