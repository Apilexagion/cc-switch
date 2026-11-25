//! Opt-in, device-local projections of a named profile into real Git checkouts.
//! A device receipt, never a file from the repository, authorizes replacement
//! and removal. Conflicting user-owned files stop the plan before any write.
use super::sync_protocol::sha256_hex;
use crate::{
    app_config::AppType,
    database::Database,
    services::{profile::ProfilePayload, SkillService},
    sync_utils::MutexExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Mutex,
};
const FILES: usize = 1500;
const TOTAL: usize = 32 * 1024 * 1024;
static LOCK: Mutex<()> = Mutex::new(());
fn er(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub root: PathBuf,
    pub app: String,
    pub profile: Option<String>,
    pub all_worktrees: bool,
    pub skill_index: bool,
}
type Owned = BTreeMap<PathBuf, BTreeMap<PathBuf, String>>;
#[derive(Default, Deserialize, Serialize)]
struct Receipt {
    binding: Option<Binding>,
    files: Owned,
}
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let o = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(er)?;
    if !o.status.success() {
        return Err("Select a local Git working tree".into());
    }
    Ok(o.stdout)
}
fn checkout_roots(root: &Path) -> Result<(PathBuf, BTreeSet<PathBuf>), String> {
    let root = root.canonicalize().map_err(er)?;
    let top = git(&root, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(
        std::str::from_utf8(&top)
            .map_err(er)?
            .trim_end_matches(['\n', '\r']),
    )
    .canonicalize()
    .map_err(er)?;
    if top != root {
        return Err("Select the root of the Git checkout, not a subdirectory".into());
    }
    let list = git(&root, &["worktree", "list", "--porcelain", "-z"])?;
    let mut trees = BTreeSet::new();
    for field in list.split(|c| *c == 0) {
        if let Some(v) = field.strip_prefix(b"worktree ") {
            let p = Path::new(std::str::from_utf8(v).map_err(er)?);
            if let Ok(c) = p.canonicalize() {
                trees.insert(c);
            }
        }
    }
    if !trees.contains(&root) || trees.len() > 32 {
        return Err("Unsupported worktree list".into());
    }
    Ok((root, trees))
}
fn safe_rel(p: &Path) -> bool {
    !p.as_os_str().is_empty()
        && p.components().all(|c| {
            let Component::Normal(v) = c else {
                return false;
            };
            let Some(s) = v.to_str() else { return false };
            if s.eq_ignore_ascii_case(".git")
                || s.contains([':', '\\', '\n', '\r'])
                || s.ends_with(['.', ' '])
            {
                return false;
            }
            let base = s.split('.').next().unwrap_or("").to_ascii_uppercase();
            !matches!(base.as_str(), "CON" | "NUL" | "PRN" | "AUX")
                && !["COM", "LPT"].iter().any(|prefix| {
                    base.strip_prefix(prefix)
                        .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
                })
        })
}
fn safe_path(root: &Path, rel: &Path, create: bool) -> Result<PathBuf, String> {
    if !safe_rel(rel) {
        return Err("Invalid checkout path".into());
    }
    let mut at = root.to_owned();
    for part in rel.parent().unwrap_or(Path::new("")).components() {
        at.push(part.as_os_str());
        match fs::symlink_metadata(&at) {
            Ok(s) if s.is_dir() && !s.file_type().is_symlink() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if create {
                    fs::create_dir(&at).map_err(er)?;
                }
            }
            _ => return Err("Refusing non-directory or linked checkout path".into()),
        }
    }
    let dest = root.join(rel);
    if fs::symlink_metadata(&dest).is_ok_and(|m| !m.file_type().is_file()) {
        return Err("Refusing linked or non-file output".into());
    }
    Ok(dest)
}
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let f = fs::File::open(path).map_err(er)?;
    if !f.metadata().map_err(er)?.is_file() {
        return Err("Expected a regular file".into());
    }
    f.take((limit + 1) as u64)
        .read_to_end(&mut data)
        .map_err(er)?;
    if data.len() > limit {
        return Err("Resource exceeds size limit".into());
    }
    Ok(data)
}
#[derive(Default)]
struct Assets {
    files: BTreeMap<PathBuf, Vec<u8>>,
    size: usize,
}
impl Assets {
    fn add(&mut self, p: PathBuf, bytes: Vec<u8>) -> Result<(), String> {
        if !safe_rel(&p)
            || self.files.contains_key(&p)
            || self.files.len() >= FILES
            || self.size + bytes.len() > TOTAL
        {
            return Err("Duplicate, invalid or oversized resource set".into());
        }
        self.size += bytes.len();
        self.files.insert(p, bytes);
        Ok(())
    }
    fn tree(&mut self, source: &Path, rel: &Path, depth: u8) -> Result<(), String> {
        if depth > 20 {
            return Err("Skill directory too deep".into());
        }
        for entry in fs::read_dir(source).map_err(er)? {
            let entry = entry.map_err(er)?;
            let name = entry.file_name();
            if name.eq_ignore_ascii_case(OsStr::new(".git")) {
                continue;
            }
            let child = rel.join(name);
            let ty = entry.file_type().map_err(er)?;
            if ty.is_dir() {
                self.tree(&entry.path(), &child, depth + 1)?;
            } else if ty.is_file() {
                self.add(child, read_bounded(&entry.path(), TOTAL - self.size)?)?;
            } else {
                return Err(
                    "Linked skill assets are not exported; use the skill index instead".into(),
                );
            }
        }
        Ok(())
    }
}
fn assets(db: &Database, b: &Binding, app: &AppType) -> Result<Assets, String> {
    let mut out = Assets::default();
    let Some(id) = &b.profile else { return Ok(out) };
    let raw = db.get_profile(id).map_err(er)?.ok_or("Profile not found")?;
    let p: ProfilePayload = serde_json::from_str(&raw.payload).map_err(|_| "Invalid profile")?;
    let claude = matches!(app, AppType::Claude);
    let ms = p
        .mcp
        .get(app)
        .and_then(Option::as_ref)
        .ok_or("Profile has not captured this client")?;
    let servers = db.get_all_mcp_servers().map_err(er)?;
    if !ms.is_empty() {
        if claude {
            let mut map = serde_json::Map::new();
            for id in ms {
                let s = servers
                    .get(id)
                    .ok_or("MCP server removed since profile capture")?;
                map.insert(s.id.clone(), s.server.clone());
            }
            out.add(
                ".mcp.json".into(),
                serde_json::to_vec_pretty(&json!({"mcpServers":map})).map_err(er)?,
            )?;
        } else {
            let mut t = toml_edit::Table::new();
            for id in ms {
                let s = servers
                    .get(id)
                    .ok_or("MCP server removed since profile capture")?;
                t.insert(
                    &s.id,
                    toml_edit::Item::Table(
                        crate::mcp::codex::json_server_to_codex_toml_table(
                            &super::mcp_tool_policy::effective("codex", id, &s.server)
                                .map_err(er)?,
                        )
                        .map_err(|_| "Invalid Codex MCP configuration")?,
                    ),
                );
            }
            let mut doc = toml_edit::DocumentMut::new();
            doc["mcp_servers"] = toml_edit::Item::Table(t);
            out.add(".codex/config.toml".into(), doc.to_string().into_bytes())?;
        }
    }
    if let Some(id) = p.prompts.get(app).and_then(Option::as_ref) {
        if let Some(prompt) = db.get_prompts(app).map_err(er)?.get(id) {
            out.add(
                if claude {
                    ".claude/rules/cc-switch-project.md"
                } else {
                    "AGENTS.md"
                }
                .into(),
                prompt.content.as_bytes().into(),
            )?;
        }
    }
    let skills = db.get_all_installed_skills().map_err(er)?;
    let from = p
        .skills
        .get(app)
        .and_then(Option::as_deref)
        .unwrap_or_default();
    let base = Path::new(if claude {
        ".claude/skills"
    } else {
        ".agents/skills"
    });
    let mut index=String::from("---\nname: cc-switch-skills\ndescription: Find project skills by name or description, then read their SKILL.md only when needed.\n---\n\n# Project skills\n");
    if !from.is_empty() {
        let source = SkillService::ssot_path().canonicalize().map_err(er)?;
        for id in from {
            let sk = skills
                .get(id)
                .ok_or("Skill removed since profile capture")?;
            let name = Path::new(&sk.directory);
            if !safe_rel(name) || name.components().count() != 1 {
                return Err("Invalid skill name".into());
            }
            let src = source.join(name).canonicalize().map_err(er)?;
            if !src.starts_with(&source) || !src.join("SKILL.md").is_file() {
                return Err("Invalid skill source".into());
            }
            if b.skill_index {
                if !super::skill::index::indexable(&src.join("SKILL.md")).map_err(er)? {
                    return Err(format!("Skill requires direct invocation: {}", sk.name));
                }
                index.push_str(&format!(
                    "\n- {}: {}\n  SKILL.md: {}\n",
                    sk.name,
                    sk.description.as_deref().unwrap_or(""),
                    src.join("SKILL.md").display()
                ));
            } else {
                out.tree(&src, &base.join(name), 0)?;
            }
        }
        if b.skill_index {
            out.add(base.join("cc-switch-skills/SKILL.md"), index.into_bytes())?;
        }
    }
    Ok(out)
}
fn content_hash(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(Some(sha256_hex(&read_bounded(path, TOTAL)?))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(er(e)),
    }
}
fn write_new(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut tmp =
        tempfile::NamedTempFile::new_in(path.parent().ok_or("Missing parent")?).map_err(er)?;
    tmp.write_all(data).map_err(er)?;
    tmp.as_file().sync_all().map_err(er)?;
    tmp.persist_noclobber(path).map_err(|e| er(e.error))?;
    Ok(())
}
pub fn project(db: &Database, b: Binding, token: Option<&str>) -> Result<Value, String> {
    project_in(db, b, token, crate::live::engine::DeviceStore::for_device())
}
fn project_in(
    db: &Database,
    b: Binding,
    token: Option<&str>,
    device: crate::live::engine::DeviceStore,
) -> Result<Value, String> {
    let _guard = LOCK.lock_recover();
    let app = match b.app.as_str() {
        "claude" => AppType::Claude,
        "codex" => AppType::Codex,
        _ => return Err("Only Claude Code and Codex have project projections".into()),
    };
    let (root, worktrees) = checkout_roots(&b.root)?;

    let receipt = device.file(&format!(
        "checkout-{}.json",
        sha256_hex(format!("{}:{}", b.app, root.display()).as_bytes())
    ));
    let mut saved: Receipt = match fs::read(&receipt) {
        Ok(buf) => serde_json::from_slice(&buf).map_err(|_| "Invalid checkout receipt")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Receipt::default(),
        Err(e) => return Err(er(e)),
    };
    let previous_profile = saved.binding.as_ref().and_then(|s| s.profile.clone());
    let data = assets(db, &b, &app)?;
    let mut changes = Vec::new();
    let mut pending = Vec::new();
    for tree in &worktrees {
        let selected = b.profile.is_some() && (b.all_worktrees || tree == &root);
        let old = saved.files.get(tree).cloned().unwrap_or_default();
        let mut keys: BTreeSet<&PathBuf> = old.keys().collect();
        if selected {
            keys.extend(data.files.keys());
        }
        for key in keys {
            let want = if selected { data.files.get(key) } else { None };
            let target = safe_path(tree, key, false)?;
            let was = content_hash(&target)?;
            let previous = old.get(key);
            if was.as_ref() != previous && was.is_some() {
                return Err(format!(
                    "Not changing an unowned or externally edited file: {}",
                    target.display()
                ));
            }
            let digest = want.map(|w| sha256_hex(w));
            if was == digest && previous == digest.as_ref() {
                continue;
            }
            if want.is_some()
                && matches!(key.to_str(), Some(".mcp.json" | ".codex/config.toml"))
                && !Command::new("git")
                    .arg("-C")
                    .arg(tree)
                    .args(["check-ignore", "-q", "--"])
                    .arg(key)
                    .status()
                    .map_err(er)?
                    .success()
            {
                return Err(format!("MCP config may contain credentials. Exclude {} in .git/info/exclude first; tracked files are never replaced.",key.display()));
            }
            changes.push(json!({"checkout":tree,"file":key,"action":if want.is_none(){"remove"}else if was.is_none(){"add"}else{"change"}}));
            pending.push((tree.clone(), key.clone(), was, digest));
        }
    }
    let fingerprint = sha256_hex(&serde_json::to_vec(&(&b, &pending, &worktrees)).map_err(er)?);
    if let Some(token) = token {
        if token != fingerprint {
            return Err("The checkout plan changed; preview again".into());
        }
        saved.binding = b.profile.as_ref().map(|_| b.clone());
        // Record each successful file before moving on, so an interrupted batch
        // can be retried/removed without claiming an untouched foreign file.
        for (tree, key, was, after) in pending {
            let path = safe_path(&tree, &key, after.is_some())?;
            if content_hash(&path)? != was {
                return Err("Checkout file changed since preview".into());
            }
            let record = saved.files.entry(tree).or_default();
            if let Some(hash) = after {
                let bytes = &data.files[&key];
                if was.is_none() {
                    write_new(&path, bytes)?;
                } else {
                    crate::config::atomic_write_private(&path, bytes).map_err(er)?;
                }
                record.insert(key, hash);
            } else {
                if was.is_some() {
                    fs::remove_file(&path).map_err(er)?;
                }
                record.remove(&key);
            }
            crate::config::atomic_write_private(&receipt, &serde_json::to_vec(&saved).map_err(er)?)
                .map_err(er)?;
        }
        crate::config::atomic_write_private(&receipt, &serde_json::to_vec(&saved).map_err(er)?)
            .map_err(er)?;
    }
    Ok(
        json!({"token":fingerprint,"changes":changes,"checkouts":worktrees,"previousProfile":previous_profile}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binding_is_scoped_to_checked_out_trees_and_never_overwrites_manual_edits() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("first");
        let other = temp.path().join("second");
        fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]).unwrap();
        git(
            &main,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        git(
            &main,
            &["worktree", "add", "-q", "--detach", other.to_str().unwrap()],
        )
        .unwrap();
        let db = Database::memory().unwrap();
        let p = serde_json::from_value(
            json!({"id":"x","name":"x","content":"project instructions","enabled":false}),
        )
        .unwrap();
        db.save_prompt(&AppType::Codex, &p).unwrap();
        let payload = json!({"mcp":{"codex":[]},"prompts":{"codex":"x"}});
        db.save_profile(&crate::database::Profile {
            id: "p".into(),
            name: "P".into(),
            payload: payload.to_string(),
            sort_order: None,
            created_at: None,
            updated_at: None,
        })
        .unwrap();
        let store = crate::live::engine::DeviceStore::at(temp.path().join("device"));
        let mut request = Binding {
            root: main.clone(),
            app: "codex".into(),
            profile: Some("p".into()),
            all_worktrees: true,
            skill_index: false,
        };
        let run =
            |b: &Binding, token: Option<&str>| project_in(&db, b.clone(), token, store.clone());
        let plan = run(&request, None).unwrap();
        assert_eq!(plan["changes"].as_array().unwrap().len(), 2);
        assert!(!main.join("AGENTS.md").exists());
        run(&request, plan["token"].as_str()).unwrap();
        assert!(other.join("AGENTS.md").is_file());
        request.all_worktrees = false;
        let plan = run(&request, None).unwrap();
        fs::write(other.join("AGENTS.md"), "manual").unwrap();
        assert!(run(&request, plan["token"].as_str()).is_err());
        assert_eq!(
            fs::read_to_string(other.join("AGENTS.md")).unwrap(),
            "manual"
        );
        fs::write(other.join("AGENTS.md"), "project instructions").unwrap();
        let plan = run(&request, None).unwrap();
        run(&request, plan["token"].as_str()).unwrap();
        assert!(!other.join("AGENTS.md").exists());
        assert!(main.join("AGENTS.md").is_file());
    }
    #[test]
    fn cannot_traverse_or_claim_git_metadata() {
        assert!(!safe_rel(Path::new("../x")));
        assert!(!safe_rel(Path::new(".GIT/config")));
        assert!(safe_rel(Path::new(".claude/skills/a/SKILL.md")));
    }
    #[test]
    fn unknown_file_is_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("AGENTS.md");
        fs::write(&p, b"mine").unwrap();
        assert!(write_new(&p, b"other").is_err());
        assert_eq!(fs::read(p).unwrap(), b"mine");
    }
}
