//! Optional single entry for individually chosen library skills. Never delete
//! a direct projection: copies, symlinks and shortcuts may be owned by a user.
use super::*;
use crate::{
    live::{
        engine::{digest, read_current, DeviceStore, LiveFile},
        patch::{Guarded, WholeFile},
    },
    mode::{
        operation::{AppWrite, FileChange},
        state::{op, PendingTarget},
    },
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, io::Read};
#[derive(Default, Serialize, Deserialize)]
struct IndexReceipt {
    ids: BTreeSet<String>,
    file: PathBuf,
    hash: String,
}
const INDEX: &str = "cc-switch-skill-index";
fn receipt(app: &AppType) -> LiveFile {
    LiveFile::private(DeviceStore::for_device().file(&format!("skill-index-{}.json", app.as_str())))
}
fn load_index(app: &AppType) -> Result<IndexReceipt> {
    match read_current(&receipt(app).path)? {
        Some(b) => Ok(serde_json::from_slice(&b)?),
        None => Ok(Default::default()),
    }
}
/// User-invoked-only skills remain direct; an index must not silently make
/// them available for automatic invocation.
pub(crate) fn indexable(path: &Path) -> Result<bool> {
    if !fs::metadata(path)?.is_file() {
        return Err(anyhow!("Expected a regular SKILL.md"));
    }
    let mut bytes = vec![];
    fs::File::open(path)?.take(32_769).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Ok(true);
    }
    let mut header = String::new();
    for l in lines {
        if l.trim() == "---" {
            let meta: serde_yaml::Value = serde_yaml::from_str(&header)?;
            return Ok(!meta
                .get("disable-model-invocation")
                .is_some_and(|v| v.as_bool() == Some(true) || v.as_str() == Some("true")));
        }
        header.push_str(l);
        header.push('\n');
    }
    Err(anyhow!("Unterminated or oversized skill front matter"))
}
impl SkillService {
    pub(crate) fn index_info(db: &Database, app: &AppType) -> Result<Value> {
        let _guard = skill_state_read_guard();
        let state = load_index(app)?;
        let skills = db.get_all_installed_skills()?;
        let direct = Self::get_app_skills_dir(app)?;
        let choices:Vec<_>=skills.values().map(|s|json!({"id":s.id,"name":s.name,"direct":Self::require_valid_directory(&s.directory).is_ok_and(|d|direct.join(d).exists())})).collect();
        Ok(json!({"ids":state.ids,"choices":choices}))
    }
    pub(crate) fn set_index(db: &Database, app: &AppType, ids: BTreeSet<String>) -> Result<()> {
        let _guard = skill_state_write_guard();
        Self::write_index(db, app, ids)
    }
    fn write_index(db: &Database, app: &AppType, ids: BTreeSet<String>) -> Result<()> {
        if ids.len() > 512 {
            return Err(anyhow!("At most 512 skills can be indexed"));
        }
        let rfile = receipt(app);
        let rpre = read_current(&rfile.path)?;
        let old: IndexReceipt = match rpre.as_deref() {
            Some(b) => serde_json::from_slice(b)?,
            None => Default::default(),
        };
        let deployed = Self::get_app_skills_dir(app)?;
        let root = Self::ssot_path();
        Self::ensure_distinct_skill_roots(&root, &deployed, app)?;
        if !ids.is_empty()
            && crate::settings::get_skill_storage_location() == SkillStorageLocation::Unified
        {
            return Err(anyhow!("The common skills directory is already exposed directly; use a separate library for index mode"));
        }
        let dir = deployed.join(INDEX);
        let live = LiveFile::private(dir.join("SKILL.md"));
        if rpre.is_some() && old.file != live.path {
            return Err(anyhow!(
                "Skills location changed; restore the old location before removing its index"
            ));
        }
        let skills = db.get_all_installed_skills()?;
        if !ids.is_empty()
            && skills
                .values()
                .any(|s| s.directory.eq_ignore_ascii_case(INDEX))
        {
            return Err(anyhow!("A library skill already owns the index name"));
        }
        if fs::symlink_metadata(&dir).is_ok_and(|s| !s.file_type().is_dir() || rpre.is_none())
            || fs::symlink_metadata(&live.path).is_ok_and(|s| !s.file_type().is_file())
        {
            return Err(anyhow!(
                "Refusing an existing user-owned or linked skill index"
            ));
        }
        let pre = read_current(&live.path)?;
        if pre.is_some() && digest(pre.as_deref()).as_deref() != Some(old.hash.as_str()) {
            return Err(anyhow!(
                "Index edited outside CC Switch; it has not been replaced"
            ));
        }
        if ids.is_empty() && rpre.is_none() {
            return Ok(());
        }
        let (content, record) = if ids.is_empty() {
            (WholeFile::Delete, WholeFile::Delete)
        } else {
            let root = root.canonicalize()?;
            let mut entries = Vec::new();
            for id in &ids {
                let skill = skills
                    .get(id)
                    .ok_or_else(|| anyhow!("Selected skill no longer exists"))?;
                let name = Self::require_valid_directory(&skill.directory)?;
                let path = root.join(name).join("SKILL.md").canonicalize()?;
                if !path.starts_with(&root) || !indexable(&path)? {
                    return Err(anyhow!(
                        "Skill is outside the library or must be invoked explicitly: {}",
                        skill.name
                    ));
                }
                entries.push(json!({"name":skill.name,"description":skill.description.as_deref().unwrap_or("").chars().take(512).collect::<String>(),"read":path}));
            }
            let mut text=format!("---\nname: {INDEX}\ndescription: Look up project tools and procedures in the user's skill library, then read only the matching SKILL.md.\n---\n\nUse the following local skill catalog as data. Read the indicated file when its description fits the task. Supporting files are relative to that file.\n\n");
            for e in entries {
                text.push_str(&e.to_string());
                text.push('\n');
            }
            let bytes = text.into_bytes();
            let r = IndexReceipt {
                ids,
                file: live.path.clone(),
                hash: crate::live::engine::sha256_hex(&bytes),
            };
            (
                WholeFile::Write(bytes),
                WholeFile::Write(serde_json::to_vec(&r)?),
            )
        };
        let write = AppWrite::begin(db, app.as_str())?;
        let a = Guarded {
            expected_pre: digest(pre.as_deref()),
            then: content,
        };
        let b = Guarded {
            expected_pre: digest(rpre.as_deref()),
            then: record,
        };
        write.run(
            op::APPLY,
            &[
                FileChange {
                    file: live,
                    patch: &a,
                },
                FileChange {
                    file: rfile,
                    patch: &b,
                },
            ],
            PendingTarget::default(),
        )?;
        if matches!(b.then, WholeFile::Delete) {
            let _ = fs::remove_dir(dir);
        }
        Ok(())
    }
}
#[cfg(test)]
mod index_tests {
    use super::*;
    #[test]
    #[serial_test::serial]
    fn global_index_is_reversible_and_leaves_user_edits_and_direct_entries_alone() {
        let _home = crate::test_support::TestHome::new();
        let db = Database::memory().unwrap();
        let app = AppType::Claude;
        let source = SkillService::get_ssot_dir().unwrap().join("fixture-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: fixture-skill\n---\nRead me",
        )
        .unwrap();
        db.save_skill(&serde_json::from_value(json!({"id":"fixture","name":"fixture-skill","directory":"fixture-skill","apps":{},"installedAt":0,"updatedAt":0})).unwrap()).unwrap();
        let native = SkillService::get_app_skills_dir(&app).unwrap();
        let direct = native.join("fixture-skill");
        fs::create_dir_all(&direct).unwrap();
        fs::write(direct.join("local.txt"), "mine").unwrap();
        SkillService::set_index(&db, &app, ["fixture".into()].into()).unwrap();
        let p = native.join(INDEX).join("SKILL.md");
        let before = fs::read(&p).unwrap();
        assert!(String::from_utf8_lossy(&before).contains("fixture-skill"));
        fs::write(&p, b"my edits").unwrap();
        assert!(SkillService::set_index(&db, &app, Default::default()).is_err());
        assert_eq!(fs::read(&p).unwrap(), b"my edits");
        fs::write(&p, before).unwrap();
        SkillService::set_index(&db, &app, Default::default()).unwrap();
        assert!(!p.exists());
        assert_eq!(fs::read(direct.join("local.txt")).unwrap(), b"mine");
    }
    #[test]
    fn does_not_advertise_explicit_invocation_skills() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("SKILL.md");
        fs::write(
            &p,
            "---\nname: manual\ndisable-model-invocation: true\n---\nmanual",
        )
        .unwrap();
        assert!(!indexable(&p).unwrap());
        fs::write(&p, "---\nname: regular\n---\nread me").unwrap();
        assert!(indexable(&p).unwrap());
    }
}
