//! WebDAV immutable-snapshot sync with legacy v2 readers and DB compatibility subdirectories.
//!
//! Implements manifest-based synchronization on top of the HTTP transport
//! primitives in [`super::webdav`]. Artifact set: `db.sql` + `skills.zip`.

use chrono::Utc;
use serde_json::Value;

use crate::error::AppError;
use crate::services::webdav::{
    auth_from_credentials, build_remote_url, ensure_remote_directories, get_bytes, path_segments,
    put_bytes_conditional, test_connection, WebDavAuth,
};
use crate::settings::{update_webdav_sync_status, WebDavSyncSettings, WebDavSyncStatus};

use super::sync_protocol::{
    build_local_snapshot, effective_db_compat_version, localized, manifest_artifact_path,
    manifest_precondition, persist_sync_success_best_effort, sha256_hex, snapshot_artifact_path,
    validate_artifact_size_limit, validate_manifest_compat, verify_artifact, ArtifactMeta,
    ManifestPrecondition, RemoteLayout, SyncManifest, DB_COMPAT_VERSION, MAX_MANIFEST_BYTES,
    MAX_SYNC_ARTIFACT_BYTES, PREVIOUS_PROTOCOL_VERSION, PROTOCOL_VERSION, REMOTE_DB_SQL,
    REMOTE_MANIFEST, REMOTE_SKILLS_ZIP,
};

#[cfg(test)]
pub(crate) fn sync_mutex() -> &'static tokio::sync::Mutex<()> {
    super::sync_protocol::sync_mutex()
}

pub(crate) mod archive;

/// Content type for the manifest PUT. Some WebDAV servers (e.g. CSTCloud
/// Data Capsule) reject multi-line bodies sent as `application/json` with a
/// size-mismatch 500, while accepting the same bytes as a generic binary.
/// The bytes are unchanged, so readers of any version are unaffected.
const MANIFEST_CONTENT_TYPE: &str = "application/octet-stream";

struct RemoteSnapshot {
    layout: RemoteLayout,
    manifest: SyncManifest,
    manifest_bytes: Vec<u8>,
    manifest_etag: Option<String>,
}
// ─── Public API ──────────────────────────────────────────────

/// Check WebDAV connectivity and ensure remote directory structure.
pub async fn check_connection(settings: &WebDavSyncSettings) -> Result<(), AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    test_connection(&settings.base_url, &auth).await?;
    let dir_segs = remote_dir_segments(settings, RemoteLayout::Current);
    ensure_remote_directories(&settings.base_url, &dir_segs, &auth).await?;
    Ok(())
}

/// Upload local snapshot (db + skills) to remote.
pub async fn upload(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
) -> Result<Value, AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    let dir_segs = remote_dir_segments(settings, RemoteLayout::Current);
    ensure_remote_directories(&settings.base_url, &dir_segs, &auth).await?;

    let snapshot = build_local_snapshot(db)?;

    let manifest_url = remote_file_url(settings, RemoteLayout::Current, REMOTE_MANIFEST)?;
    let remote = get_bytes(&manifest_url, &auth, MAX_MANIFEST_BYTES).await?;
    let condition = manifest_precondition(
        remote
            .as_ref()
            .map(|(bytes, etag)| (bytes.as_slice(), etag.as_deref())),
    );
    let mut snapshot_dir = dir_segs;
    snapshot_dir.extend(["snapshots".to_string(), snapshot.snapshot_id.clone()]);
    ensure_remote_directories(&settings.base_url, &snapshot_dir, &auth).await?;
    let db_url = remote_file_url(
        settings,
        RemoteLayout::Current,
        &snapshot_artifact_path(&snapshot.snapshot_id, REMOTE_DB_SQL),
    )?;
    put_immutable_artifact(&db_url, &auth, snapshot.db_sql, "application/sql").await?;
    let skills_url = remote_file_url(
        settings,
        RemoteLayout::Current,
        &snapshot_artifact_path(&snapshot.snapshot_id, REMOTE_SKILLS_ZIP),
    )?;
    put_immutable_artifact(&skills_url, &auth, snapshot.skills_zip, "application/zip").await?;
    // No published artifact is overwritten. A failed transfer leaves the last
    // manifest pointing to its complete snapshot, even across concurrent writers.
    let etag = put_bytes_conditional(
        &manifest_url,
        &auth,
        snapshot.manifest_bytes,
        MANIFEST_CONTENT_TYPE,
        condition.as_ref(),
    )
    .await?;

    let _persisted = persist_sync_success_best_effort(
        settings,
        snapshot.manifest_hash,
        etag,
        persist_sync_success,
    );
    Ok(serde_json::json!({ "status": "uploaded" }))
}

async fn put_immutable_artifact(
    url: &str,
    auth: &WebDavAuth,
    bytes: Vec<u8>,
    content_type: &str,
) -> Result<(), AppError> {
    let meta = ArtifactMeta {
        sha256: sha256_hex(&bytes),
        size: bytes.len() as u64,
        path: None,
    };
    match put_bytes_conditional(
        url,
        auth,
        bytes,
        content_type,
        Some(&ManifestPrecondition::Missing),
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(error) => {
            // A concurrent writer or retry may have already published this
            // content hash. Verify it without overwriting the complete object.
            match get_bytes(url, auth, meta.size as usize).await {
                Ok(Some((stored, _))) if verify_artifact(&stored, content_type, &meta).is_ok() => {
                    Ok(())
                }
                _ => Err(error),
            }
        }
    }
}

/// Download remote snapshot and apply to local database + skills.
pub(crate) async fn download(
    db: &crate::database::Database,
    settings: &mut WebDavSyncSettings,
    intent: crate::services::sync_review::Pull<'_>,
) -> Result<Value, AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    let snapshot = find_remote_snapshot(settings, &auth)
        .await?
        .ok_or_else(|| {
            localized(
                "webdav.sync.remote_empty",
                "远端没有可下载的同步数据",
                "No downloadable sync data found on the remote.",
            )
        })?;

    validate_manifest_compat(&snapshot.manifest, snapshot.layout)?;

    // Download and verify artifacts
    let db_sql = download_and_verify(
        settings,
        &auth,
        snapshot.layout,
        REMOTE_DB_SQL,
        &snapshot.manifest,
    )
    .await?;
    let skills_zip = download_and_verify(
        settings,
        &auth,
        snapshot.layout,
        REMOTE_SKILLS_ZIP,
        &snapshot.manifest,
    )
    .await?;

    if let Some(preview) = crate::services::sync_review::pull(
        db,
        &db_sql,
        &skills_zip,
        &snapshot.manifest_bytes,
        "webdav",
        intent,
    )? {
        return Ok(preview);
    }

    let manifest_hash = sha256_hex(&snapshot.manifest_bytes);
    let _persisted = persist_sync_success_best_effort(
        settings,
        manifest_hash,
        snapshot.manifest_etag,
        persist_sync_success,
    );
    Ok(serde_json::json!({
        "status": "downloaded",
        "sourceLayout": snapshot.layout.as_str(),
        "sourcePath": remote_dir_display(settings, snapshot.layout),
    }))
}

/// Fetch remote manifest info without downloading artifacts.
pub async fn fetch_remote_info(settings: &WebDavSyncSettings) -> Result<Option<Value>, AppError> {
    settings.validate()?;
    let auth = auth_for(settings);
    let Some(snapshot) = find_remote_snapshot(settings, &auth).await? else {
        return Ok(None);
    };
    let compatible = validate_manifest_compat(&snapshot.manifest, snapshot.layout).is_ok();
    let db_compat_version = effective_db_compat_version(&snapshot.manifest, snapshot.layout);

    let payload = serde_json::json!({
        "deviceName": snapshot.manifest.device_name,
        "createdAt": snapshot.manifest.created_at,
        "snapshotId": snapshot.manifest.snapshot_id,
        "version": snapshot.manifest.version,
        "protocolVersion": snapshot.manifest.version,
        "dbCompatVersion": db_compat_version,
        "compatible": compatible,
        "artifacts": snapshot.manifest.artifacts.keys().collect::<Vec<_>>(),
        "layout": snapshot.layout.as_str(),
        "remotePath": remote_dir_display(settings, snapshot.layout),
    });

    Ok(Some(payload))
}

// ─── Sync status persistence ─────────────────────────────────

fn persist_sync_success(
    settings: &mut WebDavSyncSettings,
    manifest_hash: String,
    etag: Option<String>,
) -> Result<(), AppError> {
    let status = WebDavSyncStatus {
        last_sync_at: Some(Utc::now().timestamp()),
        last_error: None,
        last_error_source: None,
        last_local_manifest_hash: Some(manifest_hash.clone()),
        last_remote_manifest_hash: Some(manifest_hash),
        last_remote_etag: etag,
    };
    settings.status = status.clone();
    update_webdav_sync_status(status)
}

async fn find_remote_snapshot(
    settings: &WebDavSyncSettings,
    auth: &WebDavAuth,
) -> Result<Option<RemoteSnapshot>, AppError> {
    if let Some(snapshot) = fetch_remote_snapshot(settings, auth, RemoteLayout::Current).await? {
        return Ok(Some(snapshot));
    }
    if let Some(snapshot) = fetch_remote_snapshot(settings, auth, RemoteLayout::Previous).await? {
        return Ok(Some(snapshot));
    }
    fetch_remote_snapshot(settings, auth, RemoteLayout::Legacy).await
}

async fn fetch_remote_snapshot(
    settings: &WebDavSyncSettings,
    auth: &WebDavAuth,
    layout: RemoteLayout,
) -> Result<Option<RemoteSnapshot>, AppError> {
    let manifest_url = remote_file_url(settings, layout, REMOTE_MANIFEST)?;
    let Some((manifest_bytes, manifest_etag)) =
        get_bytes(&manifest_url, auth, MAX_MANIFEST_BYTES).await?
    else {
        return Ok(None);
    };

    let manifest: SyncManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| AppError::Json {
            path: REMOTE_MANIFEST.to_string(),
            source: e,
        })?;

    Ok(Some(RemoteSnapshot {
        layout,
        manifest,
        manifest_bytes,
        manifest_etag,
    }))
}
// ─── Download & verify ───────────────────────────────────────

async fn download_and_verify(
    settings: &WebDavSyncSettings,
    auth: &WebDavAuth,
    layout: RemoteLayout,
    artifact_name: &str,
    manifest: &SyncManifest,
) -> Result<Vec<u8>, AppError> {
    let meta = manifest.artifacts.get(artifact_name).ok_or_else(|| {
        localized(
            "webdav.sync.manifest_missing_artifact",
            format!("manifest 中缺少 artifact: {artifact_name}"),
            format!("Manifest missing artifact: {artifact_name}"),
        )
    })?;
    validate_artifact_size_limit(artifact_name, meta.size)?;

    let path = manifest_artifact_path(manifest, artifact_name)?;
    let url = remote_file_url(settings, layout, &path)?;
    let (bytes, _) = get_bytes(&url, auth, MAX_SYNC_ARTIFACT_BYTES as usize)
        .await?
        .ok_or_else(|| {
            localized(
                "webdav.sync.remote_missing_artifact",
                format!("远端缺少 artifact 文件: {artifact_name}"),
                format!("Remote artifact file missing: {artifact_name}"),
            )
        })?;

    verify_artifact(&bytes, artifact_name, meta)?;
    Ok(bytes)
}

// ─── Remote path helpers ─────────────────────────────────────

fn remote_dir_segments(settings: &WebDavSyncSettings, layout: RemoteLayout) -> Vec<String> {
    let mut segs = Vec::new();
    segs.extend(path_segments(&settings.remote_root).map(str::to_string));
    let version = if layout == RemoteLayout::Current {
        PROTOCOL_VERSION
    } else {
        PREVIOUS_PROTOCOL_VERSION
    };
    segs.push(format!("v{version}"));
    if layout != RemoteLayout::Legacy {
        segs.push(format!("db-v{DB_COMPAT_VERSION}"));
    }
    segs.extend(path_segments(&settings.profile).map(str::to_string));
    segs
}

fn remote_file_url(
    settings: &WebDavSyncSettings,
    layout: RemoteLayout,
    file_name: &str,
) -> Result<String, AppError> {
    let mut segs = remote_dir_segments(settings, layout);
    segs.extend(path_segments(file_name).map(str::to_string));
    build_remote_url(&settings.base_url, &segs)
}

fn remote_dir_display(settings: &WebDavSyncSettings, layout: RemoteLayout) -> String {
    let segs = remote_dir_segments(settings, layout);
    format!("/{}", segs.join("/"))
}

fn auth_for(settings: &WebDavSyncSettings) -> WebDavAuth {
    auth_from_credentials(&settings.username, &settings.password)
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manifest_compare_and_swap_refuses_a_concurrent_writer() {
        use axum::{
            body::Bytes,
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::{Arc, Mutex};
        type RemoteStore = Arc<Mutex<(Vec<u8>, usize)>>;
        let store = Arc::new(Mutex::new((b"published-a".to_vec(), 1usize)));
        async fn read(State(store): State<RemoteStore>) -> (HeaderMap, Vec<u8>) {
            let (body, revision) = &*store.lock().unwrap();
            let mut headers = HeaderMap::new();
            headers.insert("etag", format!("\"{revision}\"").parse().unwrap());
            (headers, body.clone())
        }
        async fn write(
            State(store): State<RemoteStore>,
            headers: HeaderMap,
            body: Bytes,
        ) -> StatusCode {
            let mut current = store.lock().unwrap();
            let expected = format!("\"{}\"", current.1);
            if headers
                .get("if-match")
                .and_then(|value| value.to_str().ok())
                != Some(expected.as_str())
            {
                return StatusCode::PRECONDITION_FAILED;
            }
            *current = (body.to_vec(), current.1 + 1);
            StatusCode::NO_CONTENT
        }
        let app = Router::new()
            .route("/manifest", get(read).put(write))
            .with_state(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/manifest", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (_, etag) = get_bytes(&url, &None, 1024).await.unwrap().unwrap();
        let condition = ManifestPrecondition::Match(etag.unwrap());
        put_bytes_conditional(
            &url,
            &None,
            b"published-b".to_vec(),
            "application/json",
            Some(&condition),
        )
        .await
        .unwrap();
        assert!(put_bytes_conditional(
            &url,
            &None,
            b"published-c".to_vec(),
            "application/json",
            Some(&condition)
        )
        .await
        .is_err());
        assert_eq!(store.lock().unwrap().0, b"published-b");
        task.abort();
    }

    #[test]
    fn remote_dir_segments_uses_current_layout() {
        let settings = WebDavSyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..WebDavSyncSettings::default()
        };
        let segs = remote_dir_segments(&settings, RemoteLayout::Current);
        assert_eq!(segs, vec!["cc-switch-sync", "v3", "db-v6", "default"]);
    }

    #[test]
    fn remote_dir_segments_uses_legacy_layout() {
        let settings = WebDavSyncSettings {
            remote_root: "cc-switch-sync".to_string(),
            profile: "default".to_string(),
            ..WebDavSyncSettings::default()
        };
        let segs = remote_dir_segments(&settings, RemoteLayout::Legacy);
        assert_eq!(segs, vec!["cc-switch-sync", "v2", "default"]);
    }
}
