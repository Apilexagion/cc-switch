//! Consent and local rollback for verified WebDAV/S3 imports. Remote SQL is
//! staged with the same authorizer, limits and schema checks as an actual import.
use super::sync_protocol::{apply_snapshot, build_local_snapshot, sha256_hex};
use crate::{database::Database, error::AppError, sync_utils::MutexExt};
use rusqlite::types::ValueRef;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Cursor,
    sync::Mutex,
    time::Duration,
};

#[derive(Clone, Copy)]
pub(crate) enum Pull<'a> {
    Preview { upload: bool },
    Apply(Option<&'a str>),
}
struct UndoPoint {
    directory: tempfile::TempDir,
    post: String,
}
static UNDO: Mutex<Option<UndoPoint>> = Mutex::new(None);
fn err(e: impl std::fmt::Display) -> AppError {
    AppError::Message(e.to_string())
}
// The second line in our own SQL dump is the wall-clock export time. Never
// ignore a line in the body (a quoted multi-line cell can contain comments).
pub(crate) fn sql_hash(sql: &[u8]) -> String {
    let mut input = sql.splitn(3, |b| *b == b'\n');
    let first = input.next();
    let second = input.next();
    let stable = if first == Some("-- CC Switch SQLite 导出".as_bytes())
        && second.is_some_and(|s| s.starts_with("-- 生成时间: ".as_bytes()))
    {
        input.next().unwrap_or_default()
    } else {
        sql
    };
    sha256_hex(stable)
}
pub(crate) fn key_parts(sql_digest: &str, skills_digest: &str) -> String {
    sha256_hex(format!("{sql_digest}:{skills_digest}").as_bytes())
}
fn key(sql: &[u8], zip: &[u8]) -> String {
    key_parts(&sql_hash(sql), &sha256_hex(zip))
}
type Items = BTreeMap<(String, String), (String, String)>;
fn inventory(sql: &[u8], archive: &[u8]) -> Result<Items, AppError> {
    let (_file, db) = Database::stage_sync_sql(
        std::str::from_utf8(sql).map_err(err)?,
        Duration::from_secs(30),
    )?;
    let mut out = Items::new();
    for (table, app_scope) in [
        ("providers", true),
        ("prompts", true),
        ("skills", false),
        ("mcp_servers", false),
    ] {
        let mut stmt = db.prepare(&format!("SELECT * FROM {table}")).map_err(err)?;
        let count = stmt.column_count();
        let mut rows = stmt.query([]).map_err(err)?;
        while let Some(row) = rows.next().map_err(err)? {
            if out.len() >= 100_000 {
                return Err(err("Snapshot has too many entries to preview"));
            }
            let id: String = row.get("id").map_err(err)?;
            let name: String = row.get("name").map_err(err)?;
            let group = if app_scope {
                format!("{table}/{}", row.get::<_, String>("app_type").map_err(err)?)
            } else {
                table.to_owned()
            };
            let mut digest = Sha256::new();
            for i in 0..count {
                let v = row.get_ref(i).map_err(err)?;
                match v {
                    ValueRef::Null => digest.update([0]),
                    ValueRef::Integer(n) => {
                        digest.update([1]);
                        digest.update(n.to_le_bytes());
                    }
                    ValueRef::Real(n) => {
                        digest.update([2]);
                        digest.update(n.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(b) | ValueRef::Blob(b) => {
                        digest.update([if matches!(v, ValueRef::Text(_)) { 3 } else { 4 }]);
                        digest.update((b.len() as u64).to_le_bytes());
                        digest.update(b);
                    }
                }
            }
            out.insert((group, id), (name, format!("{:x}", digest.finalize())));
        }
    }
    if !archive.is_empty() {
        let mut z = zip::ZipArchive::new(Cursor::new(archive)).map_err(err)?;
        if z.len() > 100_000 {
            return Err(err("Too many skill archive entries"));
        }
        for i in 0..z.len() {
            let f = z.by_index_raw(i).map_err(err)?;
            if !f.is_dir() {
                let n = f
                    .enclosed_name()
                    .ok_or_else(|| err("Invalid skill archive path"))?
                    .to_string_lossy()
                    .into_owned();
                out.insert(
                    ("skillFiles".into(), n.clone()),
                    (n, format!("{}:{}", f.size(), f.crc32())),
                );
            }
        }
    }
    Ok(out)
}
fn differences(a: &Items, b: &Items) -> (usize, Vec<Value>) {
    let keys: BTreeSet<_> = a.keys().chain(b.keys()).collect();
    let mut count = 0;
    let mut rows = Vec::new();
    for k in keys {
        let (action, display) = match (a.get(k), b.get(k)) {
            (Some(old), Some(new)) if old.1 != new.1 => ("change", &new.0),
            (None, Some(new)) => ("add", &new.0),
            (Some(old), None) => ("remove", &old.0),
            _ => continue,
        };
        count += 1;
        if rows.len() < 500 {
            rows.push(json!({"group":k.0,"name":display.chars().take(180).collect::<String>(),"action":action}));
        }
    }
    (count, rows)
}
pub(crate) fn review_token(
    sql: &[u8],
    zip: &[u8],
    manifest: &[u8],
    destination: &str,
    upload: bool,
) -> String {
    sha256_hex(
        format!(
            "{upload}:{destination}:{}:{}",
            key(sql, zip),
            sha256_hex(manifest)
        )
        .as_bytes(),
    )
}
pub(crate) fn empty_upload(db: &Database, destination: &str) -> Result<Value, AppError> {
    let local = build_local_snapshot(db)?;
    let token = review_token(&local.db_sql, &local.skills_zip, &[], destination, true);
    let (total, changes) =
        differences(&Items::new(), &inventory(&local.db_sql, &local.skills_zip)?);
    Ok(json!({"total":total,"changes":changes,"token":token}))
}
pub(crate) fn pull(
    db: &Database,
    sql: &[u8],
    zip: &[u8],
    manifest: &[u8],
    backend: &str,
    intent: Pull<'_>,
) -> Result<Option<Value>, AppError> {
    let local = build_local_snapshot(db)?;
    let token = review_token(
        &local.db_sql,
        &local.skills_zip,
        manifest,
        backend,
        matches!(intent, Pull::Preview { upload: true }),
    );
    match intent {
        Pull::Preview { upload } => {
            let a = inventory(&local.db_sql, &local.skills_zip)?;
            let b = inventory(sql, zip)?;
            let (count, changes) = if upload {
                differences(&b, &a)
            } else {
                differences(&a, &b)
            };
            return Ok(Some(json!({"token":token,"total":count,"changes":changes})));
        }
        Pull::Apply(Some(want)) if want != token => {
            return Err(err(
                "Snapshot changed since preview. Review the download again.",
            ))
        }
        _ => {}
    }
    let dir = tempfile::tempdir().map_err(err)?;
    crate::config::atomic_write_private(&dir.path().join("database.sql"), &local.db_sql)?;
    crate::config::atomic_write_private(&dir.path().join("skills.zip"), &local.skills_zip)?;
    let after = apply_snapshot(
        db,
        sql,
        zip,
        Some((&sql_hash(&local.db_sql), &sha256_hex(&local.skills_zip))),
    )?;
    *UNDO.lock_recover() = after.map(|post| UndoPoint {
        directory: dir,
        post,
    });
    Ok(None)
}
pub(crate) fn undo(db: &Database) -> Result<(), AppError> {
    let mut slot = UNDO.lock_recover();
    let u = slot
        .as_ref()
        .ok_or_else(|| err("No download to undo in this application session"))?;
    let now = build_local_snapshot(db)?;
    if key(&now.db_sql, &now.skills_zip) != u.post {
        return Err(err("Local data changed after download; undo refused"));
    }
    let sql = fs::read(u.directory.path().join("database.sql")).map_err(err)?;
    let zip = fs::read(u.directory.path().join("skills.zip")).map_err(err)?;
    apply_snapshot(
        db,
        &sql,
        &zip,
        Some((&sql_hash(&now.db_sql), &sha256_hex(&now.skills_zip))),
    )?;
    *slot = None;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_ignores_only_the_generated_header() {
        let a = "-- CC Switch SQLite 导出\n-- 生成时间: 1\nSELECT 1";
        assert_eq!(
            key(a.as_bytes(), b"z"),
            key(a.replace("时间: 1", "时间: 2").as_bytes(), b"z")
        );
        assert_ne!(
            key(a.as_bytes(), b"z"),
            key(a.replace("SELECT 1", "SELECT 2").as_bytes(), b"z")
        );
    }
    #[test]
    fn preview_only_exposes_names_not_data() {
        let mut a = Items::new();
        let mut b = Items::new();
        let k = ("providers/claude".into(), "id".into());
        a.insert(k.clone(), ("Claude".into(), "oldsecret".into()));
        b.insert(k, ("Claude".into(), "newsecret".into()));
        let (n, rows) = differences(&a, &b);
        assert_eq!(n, 1);
        assert_eq!(rows[0]["action"], "change");
        assert!(!rows[0].to_string().contains("secret"));
    }
}
