//! A local, text-only fork into the other native CLI. Never mutate the source,
//! import instructions or replay tools. Both identity and cwd come from the
//! same native resume validator as the terminal button, not from the renderer.
use super::content::{validate_source, SourceLocation};
use chrono::{Local, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

const MAX_SOURCE: u64 = 32 * 1024 * 1024;
const MAX_LINES: usize = 100_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyResult {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
    pub project_dir: String,
    pub command: String,
}

type Words = Vec<(bool, String)>; // user = true
fn native_context(s: &str) -> bool {
    let s = s.trim_start();
    [
        "<environment_context",
        "<user_instructions",
        "<permissions instructions",
        "<collaboration_mode",
        "<turn_aborted",
        "# AGENTS.md instructions",
        "<system-reminder",
        "<local-command-",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "Caveat: The messages below were generated",
    ]
    .iter()
    .any(|prefix| s.starts_with(prefix))
}
fn block_text(v: &Value, user: bool, dst: &mut Vec<String>) {
    if let Some(s) = v.as_str() {
        if !s.trim().is_empty() && !native_context(s) {
            dst.push(s.to_owned());
        }
        return;
    }
    for b in v.as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text" | "input_text" | "output_text") => {
                if let Some(t) = b["text"].as_str() {
                    if !t.trim().is_empty() && !native_context(t) {
                        dst.push(t.to_owned());
                    }
                }
            }
            Some("tool_use") if !user => dst.push(format!(
                "[Historical tool call: {}]",
                b["name"]
                    .as_str()
                    .unwrap_or("tool")
                    .chars()
                    .take(100)
                    .collect::<String>()
            )),
            _ => {} // media, thinking, signatures and tool results are not instructions
        }
    }
}
fn append(v: &Value, words: &mut Words) {
    let user = match v["role"].as_str() {
        Some("user") => true,
        Some("assistant") => false,
        _ => return,
    };
    let mut pieces = Vec::new();
    block_text(&v["content"], user, &mut pieces);
    if !pieces.is_empty() {
        words.push((user, pieces.join("\n\n")));
    }
}
fn extract(bytes: &[u8], from: &str, id: &str) -> Result<Words, String> {
    let mut nodes = HashMap::new();
    let mut leaf = None;
    let mut words = Vec::new();
    let mut matched = false;
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if i >= MAX_LINES {
            return Err("Session has too many records".into());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let row: Value = serde_json::from_slice(line)
            .map_err(|_| format!("Unreadable session record {}", i + 1))?;
        if from == "codex" {
            let p = &row["payload"];
            match row["type"].as_str() {
                Some("session_meta") => {
                    if p["id"].as_str().or(p["session_id"].as_str()) != Some(id)
                        || p["source"].get("subagent").is_some()
                    {
                        return Err("Source identity changed or is read-only".into());
                    }
                    matched = true;
                }
                Some("response_item") => push_codex(p, &mut words),
                Some("compacted") => {
                    words.clear();
                    if let Some(history) = p["replacement_history"].as_array() {
                        for item in history {
                            push_codex(item, &mut words);
                        }
                    }
                }
                _ => {}
            }
        } else if let Some(key) = row["uuid"].as_str() {
            if row["sessionId"].as_str().is_some_and(|v| v != id) {
                return Err("Source identity changed".into());
            }
            matched |= row["sessionId"].as_str() == Some(id);
            let main = row["isSidechain"] != true;
            let message = main && matches!(row["type"].as_str(), Some("user" | "assistant"));
            if message {
                leaf = Some(key.to_owned());
            }
            let mut turn = Vec::new();
            if message && row["isMeta"] != true {
                append(&row["message"], &mut turn);
            }
            let key = key.to_string();
            nodes.insert(key, (row["parentUuid"].as_str().map(str::to_owned), turn));
        }
    }
    if !matched {
        return Err("Source session identity not found".into());
    }
    if from == "claude" {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        while let Some(id) = leaf.take() {
            if !seen.insert(id.clone()) {
                return Err("Invalid session parent cycle".into());
            }
            let Some((parent, turn)) = nodes.remove(&id) else {
                break;
            };
            chain.push(turn);
            leaf = parent;
        }
        words = chain.into_iter().rev().flatten().collect();
    }
    if !words.iter().any(|(user, _)| *user) {
        return Err("No transferable user messages in this session".into());
    }
    Ok(words)
}
fn push_codex(p: &Value, words: &mut Words) {
    match p["type"].as_str() {
        Some("message") => append(p, words),
        Some("function_call" | "custom_tool_call") => {
            let name = p["name"]
                .as_str()
                .unwrap_or("tool")
                .chars()
                .take(100)
                .collect::<String>();
            words.push((false, format!("[Historical tool call: {name}]")));
        }
        _ => {}
    }
}

fn target_dir(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    // Never follow a symlink below the target CLI's configured home.
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let mut path = fs::canonicalize(root).map_err(|e| e.to_string())?;
    for c in relative.components() {
        let Component::Normal(n) = c else {
            return Err("Invalid session directory".into());
        };
        path.push(n);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.to_string()),
        }
        if !fs::symlink_metadata(&path).is_ok_and(|s| s.file_type().is_dir()) {
            return Err("Session directory is not a real directory".into());
        }
    }
    Ok(path)
}
fn write_copy(
    home: &Path,
    to: &str,
    cwd: &str,
    model_provider: &str,
    words: Words,
) -> Result<CopyResult, String> {
    let now = Local::now();
    let stamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let id = Uuid::new_v4().to_string();
    let (relative, name) = if to == "claude" {
        let slug: String = cwd
            .encode_utf16()
            .map(|c| {
                u8::try_from(c)
                    .ok()
                    .filter(u8::is_ascii_alphanumeric)
                    .map_or('-', char::from)
            })
            .collect();
        if slug.is_empty() || slug.len() > 200 {
            return Err("Unsupported Claude project path length".into());
        }
        (Path::new("projects").join(slug), format!("{id}.jsonl"))
    } else {
        (
            Path::new("sessions").join(now.format("%Y/%m/%d").to_string()),
            format!("rollout-{}-{id}.jsonl", now.format("%Y-%m-%dT%H-%M-%S")),
        )
    };
    let dest = target_dir(home, &relative)?.join(name);
    let mut tmp = tempfile::NamedTempFile::new_in(dest.parent().expect("session parent"))
        .map_err(|e| e.to_string())?;
    {
        let mut out = std::io::BufWriter::new(tmp.as_file_mut());
        let mut put = |row: Value| -> Result<(), String> {
            serde_json::to_writer(&mut out, &row).map_err(|e| e.to_string())?;
            out.write_all(b"\n").map_err(|e| e.to_string())
        };
        if to == "codex" {
            put(
                json!({"type":"session_meta","timestamp":stamp,"payload":{"id":id,"session_id":id,"timestamp":stamp,"cwd":cwd,"originator":"codex_cli_rs","cli_version":"0.0.0","source":"cli","model_provider":model_provider}}),
            )?;
        }
        let mut parent = None;
        for (user, text) in words {
            let role = if user { "user" } else { "assistant" };
            if to == "codex" {
                put(
                    json!({"timestamp":stamp,"type":"response_item","payload":{"type":"message","role":role,"content":[{"type":if user{"input_text"}else{"output_text"},"text":text}]}}),
                )?;
                let evt = if user {
                    json!({"type":"user_message","message":text,"images":[],"local_images":[],"text_elements":[]})
                } else {
                    json!({"type":"agent_message","message":text})
                };
                put(json!({"timestamp":stamp,"type":"event_msg","payload":evt}))?;
            } else {
                let uuid = Uuid::new_v4().to_string();
                let msg = if user {
                    json!({"role":"user","content":text})
                } else {
                    json!({"id":format!("msg_{}",Uuid::new_v4().simple()),"type":"message","role":"assistant","model":"<synthetic>","content":[{"type":"text","text":text}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}})
                };
                put(
                    json!({"parentUuid":parent,"isSidechain":false,"type":role,"message":msg,"uuid":uuid,"timestamp":stamp,"userType":"external","entrypoint":"cli","cwd":cwd,"sessionId":id,"version":"2.0.0"}),
                )?;
                parent = Some(uuid);
            }
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist_noclobber(&dest)
        .map_err(|e| e.error.to_string())?;
    let command = if to == "codex" {
        format!("codex resume {id}")
    } else {
        format!("claude --resume {id}")
    };
    Ok(CopyResult {
        provider_id: to.into(),
        session_id: id,
        source_path: dest.to_string_lossy().into_owned(),
        project_dir: cwd.into(),
        command,
    })
}
pub fn copy(from: &str, id: &str, source_path: &str) -> Result<CopyResult, String> {
    let to = match from {
        "claude" => "codex",
        "codex" => "claude",
        _ => return Err("Only Claude Code and Codex primary sessions can be copied".into()),
    };
    let (_, cwd) = super::resume_session_target(from, id, source_path)?;
    let cwd = cwd
        .filter(|s| Path::new(s).is_absolute() && Path::new(s).is_dir())
        .ok_or("The original local project directory is unavailable")?;
    let SourceLocation::Path { path, .. } = validate_source(from, source_path)?.location else {
        return Err("A native file is required".into());
    };
    let mut data = Vec::new();
    crate::codex_rollout_file::open_reader(&path)
        .map_err(|e| e.to_string())?
        .take(MAX_SOURCE + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_SOURCE {
        return Err("Session exceeds the 32 MiB transfer limit".into());
    }
    let words = extract(&data, from, id)?;
    let (home, provider) = if to == "codex" {
        let cfg = crate::codex_config::read_codex_config_text()
            .map_err(|e| e.to_string())?
            .parse::<toml::Table>()
            .map_err(|_| "Invalid Codex configuration")?;
        (
            crate::codex_config::get_codex_config_dir(),
            cfg.get("model_provider")
                .and_then(toml::Value::as_str)
                .unwrap_or("openai")
                .to_owned(),
        )
    } else {
        (crate::config::get_claude_config_dir(), String::new())
    };
    write_copy(&home, to, &cwd, &provider, words)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_roundtrip_only_carries_active_public_conversation() {
        let data=br#"{"uuid":"a","sessionId":"s","type":"user","message":{"role":"user","content":"hello"}}
{"uuid":"lost","parentUuid":"a","type":"assistant","message":{"role":"assistant","content":"abandoned branch"}}
{"uuid":"b","parentUuid":"a","type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"text","text":"answer"}]}}
"#;
        let w = extract(data, "claude", "s").unwrap();
        let base = tempfile::tempdir().unwrap();
        for to in ["claude", "codex"] {
            let r = write_copy(base.path(), to, "/project", "openai", w.clone()).unwrap();
            let bytes = fs::read(r.source_path).unwrap();
            assert_eq!(
                extract(&bytes, to, &r.session_id).unwrap(),
                vec![(true, "hello".into()), (false, "answer".into())]
            );
            assert!(!String::from_utf8(bytes).unwrap().contains("private"));
        }
        assert!(extract(data, "claude", "not-this-session").is_err());
    }
}
