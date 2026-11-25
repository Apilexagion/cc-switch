use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use tempfile::{tempdir, TempDir};
use zip::write::SimpleFileOptions;
use zip::DateTime;

use crate::error::AppError;
use crate::services::skill::SkillService;

use crate::services::sync_protocol::{
    io_context_localized, localized, MAX_SYNC_ARTIFACT_BYTES, REMOTE_SKILLS_ZIP,
};

/// Maximum number of entries allowed in a zip archive.
///
/// skills.zip 打包的是用户已安装的全部技能，单个大型技能（如 ppt-master，13k+ 文件）
/// 就能超过原来的 10_000；与 `services/skill.rs` 的 `MAX_ARCHIVE_ENTRIES` 保持一致，
/// 否则技能装得上、同步却恢复不了。字节上限 `MAX_SYNC_ARTIFACT_BYTES` 仍然兜底。
const MAX_EXTRACT_ENTRIES: usize = 30_000;
const MAX_ARCHIVE_PATH_DEPTH: usize = 128;
const MAX_ARCHIVE_PATH_BYTES: usize = 4096;
const FILESYSTEM_ENTRY_BYTES: u64 = 4096;

/// Shared upload/restore allowance: count implicit parents and charge filesystem
/// overhead as well as actual streamed file contents. A locally accepted archive
/// must fit the same budget when another device restores it.
struct ArchiveBudget {
    entries: usize,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
    directories: HashSet<PathBuf>,
}

impl Default for ArchiveBudget {
    fn default() -> Self {
        Self {
            entries: 0,
            bytes: 0,
            max_entries: MAX_EXTRACT_ENTRIES,
            max_bytes: MAX_SYNC_ARTIFACT_BYTES,
            directories: HashSet::new(),
        }
    }
}

impl ArchiveBudget {
    fn reserve_entry(&mut self) -> Result<(), AppError> {
        if self.entries >= self.max_entries {
            return Err(localized(
                "webdav.sync.skills_zip_too_many_entries",
                "Skills 归档的文件和目录数量超过上限",
                "Skills archive has too many files and directories",
            ));
        }
        if self.bytes.saturating_add(FILESYSTEM_ENTRY_BYTES) > self.max_bytes {
            return Err(localized(
                "webdav.sync.skills_zip_too_large",
                "Skills 归档的内容及目录资源超过上限",
                "Skills archive contents and directory resources are too large",
            ));
        }
        self.entries += 1;
        self.bytes += FILESYSTEM_ENTRY_BYTES;
        Ok(())
    }

    fn reserve_path(&mut self, relative: &Path, directory: bool) -> Result<(), AppError> {
        let components: Vec<_> = relative.components().collect();
        if components.len() > MAX_ARCHIVE_PATH_DEPTH
            || relative.to_string_lossy().len() > MAX_ARCHIVE_PATH_BYTES
            || components
                .iter()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(localized(
                "webdav.sync.skills_zip_invalid_path",
                "Skills 归档路径越界、过长或嵌套过深",
                "Skills archive path is unsafe, too long or too deeply nested",
            ));
        }
        let mut path = PathBuf::new();
        for (index, component) in components.iter().enumerate() {
            path.push(component.as_os_str());
            if directory || index + 1 < components.len() {
                if !self.directories.contains(&path) {
                    self.reserve_entry()?;
                    self.directories.insert(path.clone());
                }
            } else {
                self.reserve_entry()?;
            }
        }
        Ok(())
    }
}

pub(crate) struct SkillsBackup {
    temporary_dir: Option<TempDir>,
    backup_dir: PathBuf,
    ssot_path: PathBuf,
    existed: bool,
}

pub(crate) fn zip_skills_ssot(dest_path: &Path) -> Result<(), AppError> {
    // Export/preview may run on a new device. A missing source means an
    // empty archive, not permission to create a live Skills directory.
    let source = SkillService::ssot_path();
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let file = fs::File::create(dest_path).map_err(|e| AppError::io(dest_path, e))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(DateTime::default());

    let mut budget = ArchiveBudget::default();
    if source.try_exists().map_err(|e| AppError::io(&source, e))? {
        let canonical_root = fs::canonicalize(&source).unwrap_or_else(|_| source.clone());
        let mut visited = HashSet::new();
        mark_visited_dir(&canonical_root, &mut visited)?;
        zip_dir_recursive(
            &canonical_root,
            &canonical_root,
            &mut writer,
            options,
            &mut visited,
            &mut budget,
        )?;
    }

    let file = writer.finish().map_err(|e| {
        localized(
            "webdav.sync.skills_zip_write_failed",
            format!("写入 skills.zip 失败: {e}"),
            format!("Failed to write skills.zip: {e}"),
        )
    })?;
    if file
        .metadata()
        .map_err(|e| AppError::io(dest_path, e))?
        .len()
        > MAX_SYNC_ARTIFACT_BYTES
    {
        return Err(localized(
            "webdav.sync.skills_zip_too_large",
            "Skills 归档压缩体超过同步上限",
            "Compressed Skills archive exceeds the sync limit",
        ));
    }
    Ok(())
}

pub(crate) fn restore_skills_zip(raw: &[u8]) -> Result<(), AppError> {
    if raw.len() as u64 > MAX_SYNC_ARTIFACT_BYTES {
        return Err(localized(
            "webdav.sync.skills_zip_too_large",
            "Skills 归档压缩体超过同步上限",
            "Compressed Skills archive exceeds the sync limit",
        ));
    }
    let tmp = tempdir().map_err(|e| {
        io_context_localized(
            "webdav.sync.skills_extract_tmpdir_failed",
            "创建 skills 解压临时目录失败",
            "Failed to create temporary directory for skills extraction",
            e,
        )
    })?;
    let zip_path = tmp.path().join(REMOTE_SKILLS_ZIP);
    fs::write(&zip_path, raw).map_err(|e| AppError::io(&zip_path, e))?;

    let file = fs::File::open(&zip_path).map_err(|e| AppError::io(&zip_path, e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| {
        localized(
            "webdav.sync.skills_zip_parse_failed",
            format!("解析 skills.zip 失败: {e}"),
            format!("Failed to parse skills.zip: {e}"),
        )
    })?;

    let extracted = tmp.path().join("skills-extracted");
    fs::create_dir_all(&extracted).map_err(|e| AppError::io(&extracted, e))?;

    if archive.len() > MAX_EXTRACT_ENTRIES {
        return Err(localized(
            "webdav.sync.skills_zip_too_many_entries",
            format!(
                "skills.zip 条目数过多（{}），上限 {MAX_EXTRACT_ENTRIES}",
                archive.len()
            ),
            format!(
                "skills.zip has too many entries ({}), limit is {MAX_EXTRACT_ENTRIES}",
                archive.len()
            ),
        ));
    }

    extract_skills_archive(&mut archive, &extracted, &mut ArchiveBudget::default())?;

    let ssot = SkillService::get_ssot_dir().map_err(|e| {
        localized(
            "webdav.sync.skills_ssot_dir_failed",
            format!("获取 Skills SSOT 目录失败: {e}"),
            format!("Failed to resolve Skills SSOT directory: {e}"),
        )
    })?;
    replace_skills_dir(Some(&extracted), &ssot, &RealSkillsFs)
}

fn extract_skills_archive<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    extracted: &Path,
    budget: &mut ArchiveBudget,
) -> Result<(), AppError> {
    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx).map_err(|e| {
            localized(
                "webdav.sync.skills_zip_entry_read_failed",
                format!("读取 ZIP 项失败: {e}"),
                format!("Failed to read ZIP entry: {e}"),
            )
        })?;
        let safe_name = entry.enclosed_name().ok_or_else(|| {
            localized(
                "webdav.sync.skills_zip_invalid_path",
                "Skills 归档包含越界路径",
                "Skills archive contains an unsafe path",
            )
        })?;
        if entry.is_symlink() {
            return Err(localized(
                "webdav.sync.skills_zip_invalid_path",
                "Skills 同步归档不接受符号链接条目",
                "Skills sync archives cannot contain symlink entries",
            ));
        }
        // Reserve every implicit parent before creating anything on disk.
        budget.reserve_path(&safe_name, entry.is_dir())?;
        let out_path = extracted.join(safe_name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| AppError::io(&out_path, e))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }
        let mut out = fs::File::create(&out_path).map_err(|e| AppError::io(&out_path, e))?;
        copy_entry_with_total_limit(
            &mut entry,
            &mut out,
            &mut budget.bytes,
            budget.max_bytes,
            &out_path,
        )?;
    }
    Ok(())
}

// Keep filesystem failures injectable without touching process-global state in tests.
trait SkillsFs {
    fn copy_dir(&self, source: &Path, destination: &Path) -> Result<(), AppError> {
        copy_dir_recursive(source, destination)
    }

    fn rename(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        fs::rename(source, destination)
    }

    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        fs::remove_dir_all(path)
    }
}

struct RealSkillsFs;
impl SkillsFs for RealSkillsFs {}

/// Stage on the SSOT filesystem before moving live skills. Only this attempt's
/// directory is ever cleaned up; an older recovery copy may be the only intact one.
fn replace_skills_dir(
    source: Option<&Path>,
    ssot: &Path,
    operations: &impl SkillsFs,
) -> Result<(), AppError> {
    if let Some(source) = source {
        // copy_dir_recursive intentionally skips missing sources in other callers.
        // A missing recovery snapshot must not be interpreted as an empty snapshot.
        if !fs::metadata(source)
            .map_err(|e| AppError::io(source, e))?
            .is_dir()
        {
            return Err(AppError::io(
                source,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Skills source is not a directory",
                ),
            ));
        }
    }
    let parent = ssot.parent().ok_or_else(|| {
        AppError::io(
            ssot,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Skills path has no parent",
            ),
        )
    })?;
    let transaction = tempfile::Builder::new()
        .prefix(".skills-restore-")
        .tempdir_in(parent)
        .map_err(|e| AppError::io(parent, e))?;
    let staged = transaction.path().join("new");
    let previous = transaction.path().join("previous");
    if let Some(source) = source {
        operations.copy_dir(source, &staged)?;
    }

    // Disable automatic cleanup BEFORE the first rename: once live skills move
    // here, even unwinding must not delete the only complete original tree.
    let transaction = transaction.keep();
    let had_live = ssot.exists();
    if had_live {
        if let Err(error) = operations.rename(ssot, &previous) {
            cleanup_skills_transaction(&transaction, operations);
            return Err(skills_rename_error(ssot, &previous, error));
        }
    }

    if source.is_some() {
        if let Err(error) = operations.rename(&staged, ssot) {
            let install_error = skills_rename_error(&staged, ssot, error);
            if had_live {
                // Do not delete a live path that appeared meanwhile. A conflict
                // must leave the original tree in its recovery directory.
                if let Err(error) = operations.rename(&previous, ssot) {
                    let rollback_error = skills_rename_error(&previous, ssot, error);
                    return Err(localized(
                        "webdav.sync.skills_replace_and_rollback_failed",
                        format!("替换 Skills 失败: {install_error}; 回滚也失败: {rollback_error}; 原 Skills 保留在 {}", previous.display()),
                        format!("Skills replacement failed: {install_error}; rollback also failed: {rollback_error}; original skills retained at {}", previous.display()),
                    ));
                }
            }
            cleanup_skills_transaction(&transaction, operations);
            return Err(install_error);
        }
    }

    // Files are committed. Cleanup failure must not report a failed restore and
    // cause apply_snapshot to skip the matching database import.
    cleanup_skills_transaction(&transaction, operations);
    Ok(())
}

fn skills_rename_error(source: &Path, destination: &Path, error: std::io::Error) -> AppError {
    io_context_localized(
        "webdav.sync.skills_rename_failed",
        format!(
            "移动 Skills 目录失败（{} → {}）",
            source.display(),
            destination.display()
        ),
        format!(
            "Failed to move skills directory ({} -> {})",
            source.display(),
            destination.display()
        ),
        error,
    )
}

fn cleanup_skills_transaction(path: &Path, operations: &impl SkillsFs) {
    if let Err(error) = operations.remove_dir_all(path) {
        // remove_dir_all may have removed only part of the directory.
        log::warn!(
            "[Sync] Skills restore cleanup failed at {}: {error}",
            path.display()
        );
    }
}

pub(crate) fn backup_current_skills() -> Result<SkillsBackup, AppError> {
    let ssot = SkillService::get_ssot_dir().map_err(|e| {
        localized(
            "webdav.sync.skills_ssot_dir_failed",
            format!("获取 Skills SSOT 目录失败: {e}"),
            format!("Failed to resolve Skills SSOT directory: {e}"),
        )
    })?;
    backup_skills_at(&ssot)
}

/// Preserve the exact tree and link identities on the same filesystem. Keep
/// the current source available until the staged replacement is committed.
fn backup_skills_at(ssot: &Path) -> Result<SkillsBackup, AppError> {
    let parent = ssot.parent().ok_or_else(|| {
        AppError::io(
            ssot,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Skills path has no parent",
            ),
        )
    })?;
    fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    let tmp = tempfile::Builder::new()
        .prefix(".skills-backup-")
        .tempdir_in(parent)
        .map_err(|e| AppError::io(parent, e))?;
    let backup_dir = tmp.path().join("skills-backup");
    let existed = fs::symlink_metadata(ssot).is_ok();
    if existed {
        copy_backup_tree(ssot, &backup_dir)?;
    }
    Ok(SkillsBackup {
        temporary_dir: Some(tmp),
        backup_dir,
        ssot_path: ssot.to_path_buf(),
        existed,
    })
}

fn copy_backup_tree(source: &Path, destination: &Path) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| AppError::io(source, error))?;
    let copy = || -> std::io::Result<()> {
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(source)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, destination)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x10 != 0 {
                    std::os::windows::fs::symlink_dir(&target, destination)?;
                } else {
                    std::os::windows::fs::symlink_file(&target, destination)?;
                }
            }
        } else if metadata.is_dir() {
            fs::create_dir(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy_backup_tree(&entry.path(), &destination.join(entry.file_name()))
                    .map_err(std::io::Error::other)?;
            }
            fs::set_permissions(destination, metadata.permissions())?;
        } else if metadata.is_file() {
            fs::copy(source, destination)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Unsupported entry in Skills backup",
            ));
        }
        Ok(())
    };
    copy().map_err(|error| AppError::io(source, error))
}

pub(crate) fn restore_skills_from_backup(backup: &SkillsBackup) -> Result<(), AppError> {
    if backup.existed {
        fs::symlink_metadata(&backup.backup_dir)
            .map_err(|e| AppError::io(&backup.backup_dir, e))?;
    }
    let parent = backup.ssot_path.parent().ok_or_else(|| {
        AppError::io(
            &backup.ssot_path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Skills path has no parent",
            ),
        )
    })?;
    let transaction = tempfile::Builder::new()
        .prefix(".skills-restore-")
        .tempdir_in(parent)
        .map_err(|e| AppError::io(parent, e))?;
    let incoming = transaction.path().join("failed-import");
    // The incoming tree may be needed if restoring the old directory fails.
    let transaction = transaction.keep();
    let had_incoming = fs::symlink_metadata(&backup.ssot_path).is_ok();
    if had_incoming {
        if let Err(error) = fs::rename(&backup.ssot_path, &incoming) {
            cleanup_skills_transaction(&transaction, &RealSkillsFs);
            return Err(skills_rename_error(&backup.ssot_path, &incoming, error));
        }
    }
    if backup.existed {
        if let Err(error) = fs::rename(&backup.backup_dir, &backup.ssot_path) {
            let restore_error = skills_rename_error(&backup.backup_dir, &backup.ssot_path, error);
            if had_incoming {
                if let Err(error) = fs::rename(&incoming, &backup.ssot_path) {
                    return Err(localized(
                        "webdav.sync.skills_replace_and_rollback_failed",
                        format!("恢复原 Skills 失败: {restore_error}; 恢复导入目录也失败: {error}; 目录保留在 {}", incoming.display()),
                        format!("Restoring original Skills failed: {restore_error}; restoring incoming Skills also failed: {error}; tree retained at {}", incoming.display()),
                    ));
                }
            }
            cleanup_skills_transaction(&transaction, &RealSkillsFs);
            return Err(restore_error);
        }
    }
    cleanup_skills_transaction(&transaction, &RealSkillsFs);
    Ok(())
}

impl Drop for SkillsBackup {
    fn drop(&mut self) {
        // An unwind or forgotten commit must never delete the only original
        // tree. Explicit commit/rollback consumes this guard and cleans it up.
        if let Some(temporary) = self.temporary_dir.take() {
            let retained = temporary.keep();
            log::warn!(
                "[Sync] Uncommitted original Skills retained at {}",
                retained.display()
            );
        }
    }
}

impl SkillsBackup {
    pub(crate) fn commit(mut self) {
        if let Some(temporary) = self.temporary_dir.take() {
            if let Err(error) = temporary.close() {
                log::warn!("[Sync] Skills backup cleanup failed after commit: {error}");
            }
        }
    }

    pub(crate) fn rollback_after_import_error(self, db_error: AppError) -> AppError {
        self.rollback_after_import_error_with(db_error, restore_skills_from_backup)
    }

    fn rollback_after_import_error_with(
        mut self,
        db_error: AppError,
        rollback: impl FnOnce(&SkillsBackup) -> Result<(), AppError>,
    ) -> AppError {
        // Retain the pre-import snapshot before rollback can move files or panic.
        // A replacement helper's recovery tree may contain the remote snapshot.
        let recovery_container = self.temporary_dir.take().map(TempDir::keep);
        if let Err(rollback_error) = rollback(&self) {
            let recovery_path = self.backup_dir.clone();
            return localized(
                "sync.db_import_and_rollback_failed",
                format!("导入数据库失败: {db_error}; 同时回滚 Skills 失败: {rollback_error}; 导入前的 Skills 快照保留在 {}", recovery_path.display()),
                format!("Database import failed: {db_error}; skills rollback also failed: {rollback_error}; pre-import skills snapshot retained at {}", recovery_path.display()),
            );
        }
        if let Some(path) = recovery_container {
            cleanup_skills_transaction(&path, &RealSkillsFs);
        }
        db_error
    }
}

fn zip_dir_recursive(
    root: &Path,
    current: &Path,
    writer: &mut zip::ZipWriter<fs::File>,
    options: SimpleFileOptions,
    visited: &mut HashSet<PathBuf>,
    budget: &mut ArchiveBudget,
) -> Result<(), AppError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(current).map_err(|e| AppError::io(current, e))? {
        let entry = entry.map_err(|e| AppError::io(current, e))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entries.len() >= MAX_EXTRACT_ENTRIES {
            return Err(localized(
                "webdav.sync.skills_zip_too_many_entries",
                "Skills 目录条目数超过同步上限",
                "Skills directory has too many entries to sync",
            ));
        }
        entries.push(entry);
    }
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        let real_path = match fs::canonicalize(&path) {
            Ok(p) if p.starts_with(root) => p,
            Ok(_) => {
                log::warn!(
                    "[WebDAV] Skipping symlink outside skills root: {}",
                    path.display()
                );
                continue;
            }
            // A link that does not resolve (dangling, or a target this process may not
            // open) cannot be archived. Falling through would open it as a file, and one
            // such entry would fail the whole upload, db.sql included (#9615).
            Err(error) if is_link(&path) => {
                log::warn!(
                    "[WebDAV] Skipping unresolvable link in skills: {}: {error}",
                    path.display()
                );
                continue;
            }
            Err(_) => path.clone(),
        };

        let is_dir = real_path.is_dir();
        // File aliases keep their own names while their contents come from real_path.
        // Directories keep canonical names to match traversal and deduplication.
        let archive_path = if is_dir { &real_path } else { &path };
        let rel = archive_path.strip_prefix(root).map_err(|e| {
            localized(
                "webdav.sync.zip_relative_path_failed",
                format!("生成 ZIP 相对路径失败: {e}"),
                format!("Failed to build relative ZIP path: {e}"),
            )
        })?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if is_dir {
            if !mark_visited_dir(&real_path, visited)? {
                log::warn!(
                    "[WebDAV] Skipping already visited directory: {}",
                    real_path.display()
                );
                continue;
            }
            budget.reserve_path(Path::new(&rel_str), true)?;
            writer
                .add_directory(format!("{rel_str}/"), options)
                .map_err(|e| {
                    localized(
                        "webdav.sync.zip_add_directory_failed",
                        format!("写入 ZIP 目录失败: {e}"),
                        format!("Failed to write ZIP directory entry: {e}"),
                    )
                })?;
            zip_dir_recursive(root, &real_path, writer, options, visited, budget)?;
        } else {
            // Only regular files are syncable; never open devices or FIFOs.
            if !fs::metadata(&real_path)
                .map_err(|e| AppError::io(&real_path, e))?
                .is_file()
            {
                continue;
            }
            budget.reserve_path(Path::new(&rel_str), false)?;
            writer.start_file(&rel_str, options).map_err(|e| {
                localized(
                    "webdav.sync.zip_start_file_failed",
                    format!("写入 ZIP 文件头失败: {e}"),
                    format!("Failed to start ZIP file entry: {e}"),
                )
            })?;
            let mut file = fs::File::open(&real_path).map_err(|e| AppError::io(&real_path, e))?;
            copy_entry_with_total_limit(
                &mut file,
                writer,
                &mut budget.bytes,
                budget.max_bytes,
                &real_path,
            )?;
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), AppError> {
    let mut visited = HashSet::new();
    copy_dir_recursive_inner(src, dest, &mut visited)
}

fn copy_dir_recursive_inner(
    src: &Path,
    dest: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), AppError> {
    if !src.exists() {
        return Ok(());
    }
    if !mark_visited_dir(src, visited)? {
        log::warn!(
            "[WebDAV] Skipping already visited copy path: {}",
            src.display()
        );
        return Ok(());
    }
    fs::create_dir_all(dest).map_err(|e| AppError::io(dest, e))?;
    for entry in fs::read_dir(src).map_err(|e| AppError::io(src, e))? {
        let entry = entry.map_err(|e| AppError::io(src, e))?;
        let path = entry.path();
        let dest_path = dest.join(entry.file_name());
        // Only copy the regular extracted staging tree. Original Skills are
        // backed up by copy_backup_tree, so no link needs dereferencing here.
        if is_link(&path) {
            continue;
        }
        if path.is_dir() {
            copy_dir_recursive_inner(&path, &dest_path, visited)?;
        } else {
            fs::copy(&path, &dest_path).map_err(|e| AppError::io(&dest_path, e))?;
        }
    }
    Ok(())
}

/// Whether the entry itself is a link: a symlink, or on Windows also a directory
/// junction (std reports both as symlinks).
fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn mark_visited_dir(path: &Path, visited: &mut HashSet<PathBuf>) -> Result<bool, AppError> {
    let canonical = fs::canonicalize(path).map_err(|e| AppError::io(path, e))?;
    Ok(visited.insert(canonical))
}

fn copy_entry_with_total_limit<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    total_bytes: &mut u64,
    max_total_bytes: u64,
    out_path: &Path,
) -> Result<u64, AppError> {
    let mut buffer = [0u8; 16 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|e| AppError::io(out_path, e))?;
        if n == 0 {
            break;
        }

        if total_bytes.saturating_add(n as u64) > max_total_bytes {
            let max_mb = max_total_bytes / 1024 / 1024;
            return Err(localized(
                "webdav.sync.skills_zip_too_large",
                format!("skills.zip 解压后体积超过上限（{max_mb} MB）"),
                format!("skills.zip extracted size exceeds limit ({max_mb} MB)"),
            ));
        }

        writer
            .write_all(&buffer[..n])
            .map_err(|e| AppError::io(out_path, e))?;
        *total_bytes += n as u64;
        written += n as u64;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::{copy_entry_with_total_limit, mark_visited_dir};
    use std::collections::HashSet;
    use std::io::Cursor;
    use std::path::Path;
    use tempfile::tempdir;

    #[cfg(unix)]
    mod zip_symlinks {
        use super::super::zip_dir_recursive;
        use super::*;
        use std::fs;
        use std::io::Read;
        use std::os::unix::fs::symlink;
        use zip::write::SimpleFileOptions;

        fn archive_dir(source: &Path, destination: &Path) -> zip::ZipArchive<fs::File> {
            let root = fs::canonicalize(source).unwrap();
            let mut writer = zip::ZipWriter::new(fs::File::create(destination).unwrap());
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .last_modified_time(zip::DateTime::default());
            let mut visited = HashSet::new();
            mark_visited_dir(&root, &mut visited).unwrap();
            zip_dir_recursive(
                &root,
                &root,
                &mut writer,
                options,
                &mut visited,
                &mut super::super::ArchiveBudget::default(),
            )
            .unwrap();
            writer.finish().unwrap();
            zip::ZipArchive::new(fs::File::open(destination).unwrap()).unwrap()
        }

        #[test]
        fn zip_preserves_file_symlink_names_and_contents() {
            let temp = tempdir().unwrap();
            let source = temp.path().join("skills");
            let skill = source.join("example");
            fs::create_dir_all(&skill).unwrap();
            fs::write(skill.join("AGENTS.md"), "shared instructions").unwrap();
            // Cover aliases sorted both before and after the target file.
            for name in ["00-alias.md", "CLAUDE.md", "GEMINI.md"] {
                symlink("AGENTS.md", skill.join(name)).unwrap();
            }

            let mut archive = archive_dir(&source, &temp.path().join("skills.zip"));
            assert_eq!(archive.len(), 5);
            for name in ["00-alias.md", "AGENTS.md", "CLAUDE.md", "GEMINI.md"] {
                let mut entry = archive.by_name(&format!("example/{name}")).unwrap();
                let mut content = String::new();
                entry.read_to_string(&mut content).unwrap();
                assert_eq!(content, "shared instructions");
                assert!(!entry.is_symlink());
            }
        }

        #[test]
        fn zip_skips_file_symlinks_outside_root() {
            let temp = tempdir().unwrap();
            let source = temp.path().join("skills");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("inside.md"), "included").unwrap();
            fs::write(temp.path().join("outside.md"), "excluded").unwrap();
            symlink("../outside.md", source.join("outside.md")).unwrap();

            let mut archive = archive_dir(&source, &temp.path().join("skills.zip"));
            assert_eq!(archive.len(), 1);
            assert_eq!(archive.by_index(0).unwrap().name(), "inside.md");
        }

        #[test]
        fn zip_keeps_canonical_directory_names_and_skips_cycles() {
            let temp = tempdir().unwrap();
            let source = temp.path().join("skills");
            let target = source.join("target");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("AGENTS.md"), "instructions").unwrap();
            symlink("target", source.join("alias")).unwrap();
            symlink("..", target.join("loop")).unwrap();

            let mut archive = archive_dir(&source, &temp.path().join("skills.zip"));
            assert_eq!(archive.len(), 2);
            assert!(archive.by_name("target/").unwrap().is_dir());
            let mut content = String::new();
            archive
                .by_name("target/AGENTS.md")
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, "instructions");
        }

        /// #9615: a link that does not resolve used to be opened as a file, and that
        /// one error failed the whole upload.
        #[test]
        fn zip_skips_links_that_do_not_resolve() {
            let temp = tempdir().unwrap();
            let source = temp.path().join("skills");
            let skill = source.join("example");
            fs::create_dir_all(&skill).unwrap();
            fs::write(skill.join("SKILL.md"), "kept").unwrap();
            // One at the top level (a skill installed as a link), one inside a skill.
            symlink("../missing-skill", source.join("broken")).unwrap();
            symlink("missing.md", skill.join("gone.md")).unwrap();

            let mut archive = archive_dir(&source, &temp.path().join("skills.zip"));
            let names: Vec<String> = (0..archive.len())
                .map(|index| archive.by_index(index).unwrap().name().to_string())
                .collect();
            assert_eq!(names, ["example/", "example/SKILL.md"]);
        }

        #[test]
        fn failed_database_import_restores_external_and_dangling_link_identity() {
            let temp = tempdir().unwrap();
            let source = temp.path().join("skills");
            fs::create_dir(&source).unwrap();
            let outside = temp.path().join("outside.md");
            fs::write(&outside, "private external marker").unwrap();
            symlink("../outside.md", source.join("external.md")).unwrap();
            symlink("missing.md", source.join("dangling.md")).unwrap();
            let backup = super::super::backup_skills_at(&source).unwrap();
            assert!(source.exists(), "backup must keep the source usable");
            fs::remove_dir_all(&source).unwrap();
            fs::create_dir(&source).unwrap();
            fs::write(source.join("incoming.md"), "remote snapshot").unwrap();
            let error = backup.rollback_after_import_error(crate::error::AppError::Message(
                "invalid database".into(),
            ));
            assert_eq!(error.to_string(), "invalid database");
            assert_eq!(
                fs::read_link(source.join("external.md")).unwrap(),
                Path::new("../outside.md")
            );
            assert_eq!(
                fs::read_link(source.join("dangling.md")).unwrap(),
                Path::new("missing.md")
            );
            assert_eq!(
                fs::read_to_string(outside).unwrap(),
                "private external marker"
            );
            assert!(!source.join("incoming.md").exists());
            let archive = archive_dir(&source, &temp.path().join("after-rollback.zip"));
            assert_eq!(
                archive.len(),
                0,
                "external contents must still be excluded from upload"
            );
        }

        #[test]
        fn copy_skips_links_that_do_not_resolve() {
            let temp = tempdir().unwrap();
            let source = temp.path().join("skills");
            let skill = source.join("example");
            fs::create_dir_all(&skill).unwrap();
            fs::write(skill.join("SKILL.md"), "kept").unwrap();
            symlink("../missing-skill", source.join("broken")).unwrap();
            symlink("missing.md", skill.join("gone.md")).unwrap();

            let backup = temp.path().join("backup");
            super::super::copy_dir_recursive(&source, &backup).unwrap();

            assert_eq!(
                fs::read_to_string(backup.join("example/SKILL.md")).unwrap(),
                "kept"
            );
            assert!(fs::symlink_metadata(backup.join("broken")).is_err());
            assert!(fs::symlink_metadata(backup.join("example/gone.md")).is_err());
        }
    }

    #[test]
    fn mark_visited_dir_tracks_canonical_duplicates() {
        let temp = tempdir().expect("tempdir");
        let dir = temp.path().join("skills");
        std::fs::create_dir_all(&dir).expect("create dir");

        let mut visited = HashSet::new();
        assert!(mark_visited_dir(&dir, &mut visited).expect("first visit"));
        assert!(!mark_visited_dir(&dir, &mut visited).expect("second visit"));
    }

    #[test]
    fn copy_entry_with_total_limit_rejects_oversized_stream_before_write() {
        let mut reader = Cursor::new(vec![1u8; 16]);
        let mut writer = Vec::new();
        let mut total_bytes = 0u64;

        let err = copy_entry_with_total_limit(
            &mut reader,
            &mut writer,
            &mut total_bytes,
            8,
            Path::new("skills-extracted/file.bin"),
        )
        .expect_err("stream larger than limit should be rejected");
        assert!(
            err.to_string().contains("too large") || err.to_string().contains("超过"),
            "unexpected error: {err}"
        );
        assert_eq!(
            writer.len(),
            0,
            "should not write when the first chunk exceeds limit"
        );
    }

    fn zip_payload(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn sync_upload_and_restore_use_the_same_whole_archive_budget() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("skills");
        std::fs::create_dir_all(source.join("example")).unwrap();
        std::fs::write(source.join("example/a.md"), "hello").unwrap();
        std::fs::write(source.join("example/b.md"), "world").unwrap();
        let root = source.canonicalize().unwrap();
        let output = temp.path().join("skills.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&output).unwrap());
        let mut upload = super::ArchiveBudget {
            max_bytes: super::FILESYSTEM_ENTRY_BYTES * 3 + 10,
            ..Default::default()
        };
        let mut visited = HashSet::new();
        mark_visited_dir(&root, &mut visited).unwrap();
        super::zip_dir_recursive(
            &root,
            &root,
            &mut writer,
            zip::write::SimpleFileOptions::default(),
            &mut visited,
            &mut upload,
        )
        .unwrap();
        writer.finish().unwrap();
        let destination = temp.path().join("restored");
        std::fs::create_dir(&destination).unwrap();
        let mut archive = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
        let mut restore = super::ArchiveBudget {
            max_bytes: upload.max_bytes,
            ..Default::default()
        };
        super::extract_skills_archive(&mut archive, &destination, &mut restore).unwrap();
        assert_eq!(
            (upload.entries, upload.bytes),
            (restore.entries, restore.bytes)
        );
        assert_eq!(restore.bytes, restore.max_bytes);
        assert_eq!(
            std::fs::read_to_string(destination.join("example/b.md")).unwrap(),
            "world"
        );
    }

    #[test]
    fn sync_upload_rejects_aggregate_payload_before_emitting_an_unrestorable_archive() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("skills");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a"), "hello").unwrap();
        std::fs::write(source.join("b"), "world").unwrap();
        let root = source.canonicalize().unwrap();
        let mut writer =
            zip::ZipWriter::new(std::fs::File::create(temp.path().join("skills.zip")).unwrap());
        let mut budget = super::ArchiveBudget {
            max_bytes: super::FILESYSTEM_ENTRY_BYTES * 2 + 8,
            ..Default::default()
        };
        let mut visited = HashSet::new();
        mark_visited_dir(&root, &mut visited).unwrap();
        assert!(super::zip_dir_recursive(
            &root,
            &root,
            &mut writer,
            zip::write::SimpleFileOptions::default(),
            &mut visited,
            &mut budget,
        )
        .is_err());
        assert!(budget.bytes <= budget.max_bytes);
    }

    #[test]
    fn sync_restore_budgets_implicit_parents_before_making_directories() {
        let temp = tempdir().unwrap();
        let bytes = zip_payload(&[("a/b/c/empty", b"")]);
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut budget = super::ArchiveBudget {
            max_entries: 2,
            ..Default::default()
        };
        assert!(super::extract_skills_archive(&mut archive, temp.path(), &mut budget).is_err());
        assert!(!temp.path().join("a").exists());
        assert_eq!(budget.entries, 2);
    }

    #[test]
    fn sync_restore_rejects_excessive_depth_before_touching_the_tree() {
        let temp = tempdir().unwrap();
        let name = format!("{}file", "a/".repeat(super::MAX_ARCHIVE_PATH_DEPTH));
        let bytes = zip_payload(&[(&name, b"")]);
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert!(
            super::extract_skills_archive(&mut archive, temp.path(), &mut Default::default())
                .is_err()
        );
        assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());
    }

    #[derive(Default)]
    struct FaultySkillsFs {
        rename_calls: std::cell::Cell<usize>,
        fail_renames: Vec<usize>,
        fail_copy: bool,
        fail_cleanup: bool,
        panic_on_install: bool,
        create_conflict_on_install: bool,
    }

    impl super::SkillsFs for FaultySkillsFs {
        fn copy_dir(
            &self,
            source: &Path,
            destination: &Path,
        ) -> Result<(), crate::error::AppError> {
            if self.fail_copy {
                std::fs::create_dir_all(destination).unwrap();
                std::fs::write(destination.join("partial.txt"), "partial").unwrap();
                return Err(crate::error::AppError::io(
                    destination,
                    std::io::Error::other("injected stage copy failure"),
                ));
            }
            super::copy_dir_recursive(source, destination)
        }

        fn rename(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
            let call = self.rename_calls.get() + 1;
            self.rename_calls.set(call);
            if call == 2 {
                assert!(
                    !self.panic_on_install,
                    "injected panic after moving live skills"
                );
                if self.create_conflict_on_install {
                    std::fs::create_dir_all(destination)?;
                    std::fs::write(destination.join("concurrent.txt"), "concurrent")?;
                }
            }
            if self.fail_renames.contains(&call) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("injected rename failure {call}"),
                ));
            }
            std::fs::rename(source, destination)
        }

        fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
            if self.fail_cleanup {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected cleanup failure",
                ));
            }
            std::fs::remove_dir_all(path)
        }
    }

    struct RestoreFixture {
        root: tempfile::TempDir,
        source: std::path::PathBuf,
        live: std::path::PathBuf,
    }

    impl RestoreFixture {
        fn new() -> Self {
            let root = tempdir().unwrap();
            let source = root.path().join("source");
            let live = root.path().join("skills");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("remote.txt"), "remote skills").unwrap();
            std::fs::create_dir(&live).unwrap();
            std::fs::write(live.join("original.txt"), "original skills").unwrap();
            Self { root, source, live }
        }

        fn transactions(&self) -> Vec<std::path::PathBuf> {
            std::fs::read_dir(self.root.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".skills-restore-")
                })
                .collect()
        }

        fn assert_original(&self) {
            assert_eq!(
                std::fs::read_to_string(self.live.join("original.txt")).unwrap(),
                "original skills"
            );
            assert!(!self.live.join("remote.txt").exists());
        }

        fn backup(&self) -> super::SkillsBackup {
            super::backup_skills_at(&self.live).unwrap()
        }
    }

    #[test]
    fn skills_restore_staging_failure_keeps_live_and_cleans_partial_stage() {
        let fixture = RestoreFixture::new();
        let operations = FaultySkillsFs {
            fail_copy: true,
            ..Default::default()
        };
        let error = super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations)
            .unwrap_err();
        assert!(error.to_string().contains("injected stage copy failure"));
        fixture.assert_original();
        assert_eq!(operations.rename_calls.get(), 0);
        assert!(fixture.transactions().is_empty());
    }

    #[test]
    fn skills_restore_first_rename_failure_keeps_live_and_reports_both_paths() {
        let fixture = RestoreFixture::new();
        let operations = FaultySkillsFs {
            fail_renames: vec![1],
            ..Default::default()
        };
        let error = super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations)
            .unwrap_err()
            .to_string();
        fixture.assert_original();
        assert!(error.contains(&fixture.live.display().to_string()));
        assert!(error.contains("previous"));
        assert!(error.contains("injected rename failure 1"));
        assert_eq!(operations.rename_calls.get(), 1);
        assert!(fixture.transactions().is_empty());
    }

    #[test]
    fn skills_restore_install_failure_rolls_back_original() {
        let fixture = RestoreFixture::new();
        let operations = FaultySkillsFs {
            fail_renames: vec![2],
            ..Default::default()
        };
        let error = super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations)
            .unwrap_err();
        assert!(error.to_string().contains("injected rename failure 2"));
        fixture.assert_original();
        assert_eq!(operations.rename_calls.get(), 3);
        assert!(fixture.transactions().is_empty());
    }

    #[test]
    fn skills_restore_failed_rollback_retains_original_across_retry() {
        let fixture = RestoreFixture::new();
        let operations = FaultySkillsFs {
            fail_renames: vec![2, 3],
            ..Default::default()
        };
        let error = super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations)
            .unwrap_err()
            .to_string();
        let transactions = fixture.transactions();
        assert_eq!(transactions.len(), 1);
        let recovery = transactions[0].join("previous");
        assert!(error.contains(&recovery.display().to_string()));
        assert!(error.contains("injected rename failure 2"));
        assert!(error.contains("injected rename failure 3"));
        assert!(!fixture.live.exists());
        assert_eq!(
            std::fs::read_to_string(recovery.join("original.txt")).unwrap(),
            "original skills"
        );
        // get_ssot_dir recreates an absent live directory on the next attempt.
        std::fs::create_dir(&fixture.live).unwrap();
        std::fs::write(fixture.live.join("partial.txt"), "partial").unwrap();
        let failure = FaultySkillsFs {
            fail_copy: true,
            ..Default::default()
        };
        assert!(super::replace_skills_dir(Some(&fixture.source), &fixture.live, &failure).is_err());
        super::replace_skills_dir(Some(&fixture.source), &fixture.live, &super::RealSkillsFs)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(recovery.join("original.txt")).unwrap(),
            "original skills"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.live.join("remote.txt")).unwrap(),
            "remote skills"
        );
    }

    #[test]
    fn skills_restore_conflicting_live_path_is_not_deleted_during_rollback() {
        let fixture = RestoreFixture::new();
        let operations = FaultySkillsFs {
            create_conflict_on_install: true,
            ..Default::default()
        };
        assert!(
            super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations).is_err()
        );
        assert_eq!(
            std::fs::read_to_string(fixture.live.join("concurrent.txt")).unwrap(),
            "concurrent"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.transactions()[0].join("previous/original.txt"))
                .unwrap(),
            "original skills"
        );
    }

    #[test]
    fn skills_restore_unwind_after_live_move_does_not_delete_original() {
        let fixture = RestoreFixture::new();
        let operations = FaultySkillsFs {
            panic_on_install: true,
            ..Default::default()
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations)
        }));
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.transactions()[0].join("previous/original.txt"))
                .unwrap(),
            "original skills"
        );
    }

    #[test]
    fn skills_restore_successful_install_cleanup_failure_still_succeeds() {
        let fixture = RestoreFixture::new();
        let operations = FaultySkillsFs {
            fail_cleanup: true,
            ..Default::default()
        };
        super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations).unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.live.join("remote.txt")).unwrap(),
            "remote skills"
        );
        assert_eq!(fixture.transactions().len(), 1);
    }

    #[test]
    fn skills_restore_never_touches_legacy_backup() {
        let fixture = RestoreFixture::new();
        let legacy = fixture.live.with_extension("bak");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("only-copy.txt"), "recover me").unwrap();
        // An open handle can also prevent cleanup on Windows; no operation should
        // address this backup, regardless of platform-specific sharing semantics.
        let _held_file = std::fs::File::open(legacy.join("only-copy.txt")).unwrap();
        super::replace_skills_dir(Some(&fixture.source), &fixture.live, &super::RealSkillsFs)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(legacy.join("only-copy.txt")).unwrap(),
            "recover me"
        );
    }

    #[test]
    fn skills_restore_missing_or_file_source_does_not_replace_live() {
        let fixture = RestoreFixture::new();
        for source in [
            fixture.root.path().join("missing"),
            fixture.source.join("remote.txt"),
        ] {
            assert!(
                super::replace_skills_dir(Some(&source), &fixture.live, &super::RealSkillsFs)
                    .is_err()
            );
            fixture.assert_original();
        }
        assert!(fixture.transactions().is_empty());
    }

    #[test]
    fn skills_restore_supports_absent_live_and_absent_snapshot() {
        let fixture = RestoreFixture::new();
        std::fs::remove_dir_all(&fixture.live).unwrap();
        super::replace_skills_dir(Some(&fixture.source), &fixture.live, &super::RealSkillsFs)
            .unwrap();
        assert!(fixture.live.join("remote.txt").exists());
        super::replace_skills_dir(None, &fixture.live, &super::RealSkillsFs).unwrap();
        assert!(!fixture.live.exists());
        assert!(fixture.transactions().is_empty());
    }

    #[test]
    fn skills_restore_failed_install_without_live_leaves_no_live() {
        let fixture = RestoreFixture::new();
        std::fs::remove_dir_all(&fixture.live).unwrap();
        let operations = FaultySkillsFs {
            fail_renames: vec![1],
            ..Default::default()
        };
        assert!(
            super::replace_skills_dir(Some(&fixture.source), &fixture.live, &operations).is_err()
        );
        assert!(!fixture.live.exists());
        assert!(fixture.transactions().is_empty());
    }

    #[test]
    fn skills_import_rollback_success_restores_snapshot_and_returns_database_error() {
        let fixture = RestoreFixture::new();
        let backup = fixture.backup();
        let temporary_backup = backup.temporary_dir.as_ref().unwrap().path().to_path_buf();
        super::replace_skills_dir(Some(&fixture.source), &fixture.live, &super::RealSkillsFs)
            .unwrap();
        let error = backup.rollback_after_import_error(crate::error::AppError::Message(
            "database import failed".into(),
        ));
        assert_eq!(error.to_string(), "database import failed");
        fixture.assert_original();
        assert!(!temporary_backup.exists());
    }

    #[test]
    fn skills_import_failed_rollback_preserves_pre_import_snapshot_after_drop() {
        let fixture = RestoreFixture::new();
        let backup = fixture.backup();
        let original_snapshot = backup.backup_dir.clone();
        let temporary_backup = backup.temporary_dir.as_ref().unwrap().path().to_path_buf();
        super::replace_skills_dir(Some(&fixture.source), &fixture.live, &super::RealSkillsFs)
            .unwrap();
        let operations = FaultySkillsFs {
            fail_renames: vec![2, 3],
            ..Default::default()
        };
        let error = backup
            .rollback_after_import_error_with(
                crate::error::AppError::Message("database import failed".into()),
                |backup| {
                    super::replace_skills_dir(
                        Some(&backup.backup_dir),
                        &backup.ssot_path,
                        &operations,
                    )
                },
            )
            .to_string();
        assert!(error.contains("database import failed"));
        assert!(error.contains(&original_snapshot.display().to_string()));
        assert!(error.contains("pre-import"));
        assert_eq!(
            std::fs::read_to_string(original_snapshot.join("original.txt")).unwrap(),
            "original skills"
        );
        // The helper's recovery tree is the remote snapshot, not the original one.
        assert_eq!(
            std::fs::read_to_string(fixture.transactions()[0].join("previous/remote.txt")).unwrap(),
            "remote skills"
        );
        std::fs::remove_dir_all(temporary_backup).unwrap();
    }
    #[test]
    fn skills_import_rollback_unwind_preserves_pre_import_snapshot() {
        let fixture = RestoreFixture::new();
        let backup = fixture.backup();
        let original_snapshot = backup.backup_dir.clone();
        let temporary_backup = backup.temporary_dir.as_ref().unwrap().path().to_path_buf();
        super::replace_skills_dir(Some(&fixture.source), &fixture.live, &super::RealSkillsFs)
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            backup.rollback_after_import_error_with(
                crate::error::AppError::Message("database import failed".into()),
                |_| panic!("injected rollback panic"),
            )
        }));
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(original_snapshot.join("original.txt")).unwrap(),
            "original skills"
        );
        std::fs::remove_dir_all(temporary_backup).unwrap();
    }
}
