use regex::Regex;
use std::io::Read;
use std::ops::Range;
use std::sync::LazyLock;
use tauri::AppHandle;

use crate::config::write_text_file;
use crate::openclaw_config::get_openclaw_dir;

/// Allowed workspace filenames (whitelist for security)
const ALLOWED_FILES: &[&str] = &[
    "AGENTS.md",
    "SOUL.md",
    "USER.md",
    "IDENTITY.md",
    "TOOLS.md",
    "MEMORY.md",
    "HEARTBEAT.md",
    "BOOTSTRAP.md",
    "BOOT.md",
];

fn validate_filename(filename: &str) -> Result<(), String> {
    if !ALLOWED_FILES.contains(&filename) {
        return Err(format!(
            "Invalid workspace filename: {filename}. Allowed: {}",
            ALLOWED_FILES.join(", ")
        ));
    }
    Ok(())
}

// --- Daily memory files (memory/YYYY-MM-DD.md) ---

static DAILY_MEMORY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}\.md$").unwrap());

fn validate_daily_memory_filename(filename: &str) -> Result<(), String> {
    if !DAILY_MEMORY_RE.is_match(filename) {
        return Err(format!(
            "Invalid daily memory filename: {filename}. Expected: YYYY-MM-DD.md"
        ));
    }
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyMemoryFileInfo {
    pub filename: String,
    pub date: String,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub preview: String,
}

// --- Daily memory commands ---

/// List all daily memory files under `workspace/memory/`.
#[tauri::command]
pub async fn list_daily_memory_files() -> Result<Vec<DailyMemoryFileInfo>, String> {
    let memory_dir = get_openclaw_dir().join("workspace").join("memory");

    if !memory_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files: Vec<DailyMemoryFileInfo> = Vec::new();

    let entries = std::fs::read_dir(&memory_dir)
        .map_err(|e| format!("Failed to read memory directory: {e}"))?;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }

        let date = name.trim_end_matches(".md").to_string();

        let size_bytes = meta.len();
        let modified_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let preview = read_preview(&entry.path(), 200).unwrap_or_default();

        files.push(DailyMemoryFileInfo {
            filename: name,
            date,
            size_bytes,
            modified_at,
            preview,
        });
    }

    // Sort by filename descending (newest date first, YYYY-MM-DD.md)
    files.sort_by(|a, b| b.filename.cmp(&a.filename));

    Ok(files)
}

/// Read a daily memory file.
#[tauri::command]
pub async fn read_daily_memory_file(filename: String) -> Result<Option<String>, String> {
    validate_daily_memory_filename(&filename)?;

    let path = get_openclaw_dir()
        .join("workspace")
        .join("memory")
        .join(&filename);

    if !path.exists() {
        return Ok(None);
    }

    std::fs::read_to_string(&path)
        .map(Some)
        .map_err(|e| format!("Failed to read daily memory file {filename}: {e}"))
}

/// Write a daily memory file (atomic write).
#[tauri::command]
pub async fn write_daily_memory_file(filename: String, content: String) -> Result<(), String> {
    validate_daily_memory_filename(&filename)?;

    let memory_dir = get_openclaw_dir().join("workspace").join("memory");

    std::fs::create_dir_all(&memory_dir)
        .map_err(|e| format!("Failed to create memory directory: {e}"))?;

    let path = memory_dir.join(&filename);

    write_text_file(&path, &content)
        .map_err(|e| format!("Failed to write daily memory file {filename}: {e}"))
}

/// Find the largest index `<= i` that is a valid UTF-8 char boundary.
/// Equivalent to the unstable `str::floor_char_boundary` (stabilized in 1.91).
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find the smallest index `>= i` that is a valid UTF-8 char boundary.
/// Equivalent to the unstable `str::ceil_char_boundary` (stabilized in 1.91).
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn read_preview(path: &std::path::Path, max_chars: usize) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(max_chars.saturating_mul(4) as u64)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes)
        .chars()
        .take(max_chars)
        .collect())
}

/// Case conversion can expand or shrink UTF-8. Translate only the first match
/// back to original character boundaries instead of slicing with folded offsets.
fn original_match_range(content: &str, folded: Range<usize>) -> Range<usize> {
    let mut folded_pos = 0;
    let mut original_start = None;
    for (offset, ch) in content.char_indices() {
        let next = folded_pos + ch.to_lowercase().map(char::len_utf8).sum::<usize>();
        if original_start.is_none() && folded.start < next {
            original_start = Some(offset);
        }
        if folded.end <= next {
            return original_start.unwrap_or(offset)..offset + ch.len_utf8();
        }
        folded_pos = next;
    }
    original_start.unwrap_or(content.len())..content.len()
}

fn memory_matches(content: &str, query_lower: &str) -> (Option<Range<usize>>, usize) {
    let content_lower = content.to_lowercase();
    let mut matches = content_lower.match_indices(query_lower);
    let first = matches
        .next()
        .map(|(offset, matched)| original_match_range(content, offset..offset + matched.len()));
    let count = usize::from(first.is_some()) + matches.count();
    (first, count)
}

/// Search result for daily memory full-text search.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyMemorySearchResult {
    pub filename: String,
    pub date: String,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub snippet: String,
    pub match_count: usize,
}

/// Full-text search across all daily memory files.
///
/// Performs case-insensitive search on both the date field and file content.
/// Returns results sorted by filename descending (newest first), each with a
/// snippet showing ~120 characters of context around the first match.
#[tauri::command]
pub async fn search_daily_memory_files(
    query: String,
) -> Result<Vec<DailyMemorySearchResult>, String> {
    let memory_dir = get_openclaw_dir().join("workspace").join("memory");

    if !memory_dir.exists() || query.is_empty() {
        return Ok(Vec::new());
    }

    let query_lower = query.to_lowercase();
    let mut results: Vec<DailyMemorySearchResult> = Vec::new();

    let entries = std::fs::read_dir(&memory_dir)
        .map_err(|e| format!("Failed to read memory directory: {e}"))?;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };

        let date = name.trim_end_matches(".md").to_string();
        let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let (first_match, match_count) = memory_matches(&content, &query_lower);

        // Also check date field
        let date_matches = date.to_lowercase().contains(&query_lower);

        if first_match.is_none() && !date_matches {
            continue;
        }

        // Build snippet around first content match (~120 chars of context)
        let snippet = if let Some(first_match) = first_match {
            let first_pos = first_match.start;
            let start = if first_pos > 50 {
                floor_char_boundary(&content, first_pos - 50)
            } else {
                0
            };
            let end = ceil_char_boundary(&content, (first_pos + 70).min(content.len()));
            let mut s = String::new();
            if start > 0 {
                s.push_str("...");
            }
            s.push_str(&content[start..end]);
            if end < content.len() {
                s.push_str("...");
            }
            s
        } else {
            // Date-only match — use beginning of file as preview
            let end = ceil_char_boundary(&content, 120.min(content.len()));
            let mut s = content[..end].to_string();
            if end < content.len() {
                s.push_str("...");
            }
            s
        };

        let size_bytes = meta.len();
        let modified_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        results.push(DailyMemorySearchResult {
            filename: name,
            date,
            size_bytes,
            modified_at,
            snippet,
            match_count,
        });
    }

    // Sort by filename descending (newest date first)
    results.sort_by(|a, b| b.filename.cmp(&a.filename));

    Ok(results)
}

/// Delete a daily memory file (idempotent).
#[tauri::command]
pub async fn delete_daily_memory_file(filename: String) -> Result<(), String> {
    validate_daily_memory_filename(&filename)?;

    let path = get_openclaw_dir()
        .join("workspace")
        .join("memory")
        .join(&filename);

    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| format!("Failed to delete daily memory file {filename}: {e}"))?;
    }

    Ok(())
}

// --- Workspace file commands ---

/// Read an OpenClaw workspace file content.
/// Returns None if the file does not exist.
#[tauri::command]
pub async fn read_workspace_file(filename: String) -> Result<Option<String>, String> {
    validate_filename(&filename)?;

    let path = get_openclaw_dir().join("workspace").join(&filename);

    if !path.exists() {
        return Ok(None);
    }

    std::fs::read_to_string(&path)
        .map(Some)
        .map_err(|e| format!("Failed to read workspace file {filename}: {e}"))
}

/// Write content to an OpenClaw workspace file (atomic write).
/// Creates the workspace directory if it does not exist.
#[tauri::command]
pub async fn write_workspace_file(filename: String, content: String) -> Result<(), String> {
    validate_filename(&filename)?;

    let workspace_dir = get_openclaw_dir().join("workspace");

    // Ensure workspace directory exists
    std::fs::create_dir_all(&workspace_dir)
        .map_err(|e| format!("Failed to create workspace directory: {e}"))?;

    let path = workspace_dir.join(&filename);

    write_text_file(&path, &content)
        .map_err(|e| format!("Failed to write workspace file {filename}: {e}"))
}

/// Open the workspace or memory directory in the system file manager.
/// `subdir`: "workspace" opens `~/.openclaw/workspace/`,
///           "memory" opens `~/.openclaw/workspace/memory/`.
#[tauri::command]
pub async fn open_workspace_directory(handle: AppHandle, subdir: String) -> Result<bool, String> {
    let dir = match subdir.as_str() {
        "memory" => get_openclaw_dir().join("workspace").join("memory"),
        _ => get_openclaw_dir().join("workspace"),
    };

    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    crate::host_open::open_path(&handle, &dir)
        .map_err(|e| format!("Failed to open directory: {e}"))?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{memory_matches, read_preview};

    #[test]
    fn memory_matches_return_original_offsets_after_unicode_case_expansion() {
        for prefix in ["İ".repeat(100), "K".repeat(100), "İK".repeat(100)] {
            let content = format!("{prefix}needle NEEDLE");
            let (first, count) = memory_matches(&content, "needle");
            let first = first.unwrap();
            assert_eq!(&content[first], "needle");
            assert_eq!(count, 2);
        }
    }

    #[test]
    fn memory_matches_cover_the_original_character_when_case_expands() {
        let content = "before İ after";
        let (first, count) = memory_matches(content, "i\u{307}");
        assert_eq!(&content[first.unwrap()], "İ");
        assert_eq!(count, 1);
        assert_eq!(memory_matches(content, "missing"), (None, 0));
    }

    #[test]
    fn memory_preview_reads_only_the_requested_character_prefix() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), format!("{}tail", "🙂İK".repeat(300))).unwrap();
        assert_eq!(read_preview(temp.path(), 3).unwrap(), "🙂İK");
    }
}
