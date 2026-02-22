//! Migration Progress Tracking Service — Ticket 9
//!
//! Persists per-file migration status across conversations using a standalone
//! Redb database (`migration_progress.redb`) that lives alongside the graph
//! database.  The service is intentionally self-contained: it owns its own
//! `Database` handle rather than reusing the GraphStore, which avoids invasive
//! changes to the shared graph schema.
//!
//! # Table layout
//! ```text
//! migration_status: &str → &str
//!   key:   "{project_id}\0{file_path}"   (null-byte composite key)
//!   value: JSON-serialized FileStatus
//! ```

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Table definition ────────────────────────────────────────────────────────

/// Key: `"{project_id}\0{file_path}"` — null byte acts as separator because
/// neither component is allowed to contain a null byte in practice.
/// Value: JSON-serialized [`FileStatus`].
const MIGRATION_STATUS_TABLE: TableDefinition<&str, &str> =
    TableDefinition::new("migration_status");

// ─── Public types ─────────────────────────────────────────────────────────────

/// Migration lifecycle state for a single file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MigrationStatus {
    NotStarted,
    InProgress,
    Migrated,
    Verified,
    Blocked,
}

impl MigrationStatus {
    /// Human-readable label used in reports.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::InProgress => "in_progress",
            Self::Migrated => "migrated",
            Self::Verified => "verified",
            Self::Blocked => "blocked",
        }
    }

    /// Whether this status counts toward completion percentage.
    fn is_complete(&self) -> bool {
        matches!(self, Self::Migrated | Self::Verified)
    }
}

impl std::fmt::Display for MigrationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Detailed status record for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatus {
    /// Repository-relative or absolute path to the file.
    pub file_path: String,
    /// Current migration lifecycle state.
    pub status: MigrationStatus,
    /// Free-form notes for this file (PR links, comments, etc.).
    pub notes: String,
    /// Epoch milliseconds of the last status update.
    pub last_updated: u64,
    /// Optional risk score (0–100).  Lower = safer to migrate next.
    pub risk_score: Option<u8>,
    /// Human-readable reason the file is blocked, if applicable.
    pub blocked_reason: Option<String>,
    /// File paths that must be migrated before this one can proceed.
    pub blocking_dependencies: Vec<String>,
}

/// Aggregate migration progress for a project.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationProgress {
    pub project_id: String,
    pub total_files: usize,
    pub not_started: usize,
    pub in_progress: usize,
    pub migrated: usize,
    pub verified: usize,
    pub blocked: usize,
    /// `(migrated + verified) / total * 100.0`; 0.0 when total == 0.
    pub completion_pct: f64,
    /// Progress broken down by file extension.
    pub by_file_type: HashMap<String, TypeProgress>,
    /// Files currently in the `Blocked` state.
    pub blocked_items: Vec<BlockedItem>,
    /// Up to 10 most recently updated files (newest first).
    pub recently_updated: Vec<FileStatusSummary>,
    /// Up to 5 `NotStarted` files with the lowest risk_score (easiest next).
    pub suggested_next: Vec<String>,
}

/// Per-file-type progress summary.
#[derive(Debug, Clone, Serialize)]
pub struct TypeProgress {
    pub total: usize,
    /// Files in `Migrated` or `Verified` state.
    pub completed: usize,
    pub pct: f64,
}

/// A file that is currently blocked.
#[derive(Debug, Clone, Serialize)]
pub struct BlockedItem {
    pub file_path: String,
    pub reason: String,
    pub blocking_deps: Vec<String>,
}

/// Lightweight status summary used in the `recently_updated` list.
#[derive(Debug, Clone, Serialize)]
pub struct FileStatusSummary {
    pub file_path: String,
    pub status: String,
    pub notes: String,
}

// ─── Store ───────────────────────────────────────────────────────────────────

/// Standalone Redb-backed store for migration progress.
///
/// The `Database` is wrapped in `Arc` so the store is cheap to clone.
#[derive(Clone)]
pub struct MigrationProgressStore {
    db: Arc<Database>,
}

impl MigrationProgressStore {
    // ── Lifecycle ──────────────────────────────────────────────────────────

    /// Open (or create) the migration progress database at `db_path`.
    ///
    /// The parent directory is created automatically if it does not exist.
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(db_path)?;
        // Eagerly create the table so later read transactions never see "table
        // not found" errors on an empty database.
        let wtx = db.begin_write()?;
        {
            let _ = wtx.open_table(MIGRATION_STATUS_TABLE)?;
        }
        wtx.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    // ── Writes ─────────────────────────────────────────────────────────────

    /// Insert or update the migration status for a single file.
    pub fn update_status(
        &self,
        project_id: &str,
        file_path: &str,
        status: MigrationStatus,
        notes: &str,
        risk_score: Option<u8>,
        blocked_reason: Option<&str>,
        blocking_deps: Vec<String>,
    ) -> anyhow::Result<()> {
        let key = composite_key(project_id, file_path);
        let record = FileStatus {
            file_path: file_path.to_string(),
            status,
            notes: notes.to_string(),
            last_updated: now_millis(),
            risk_score,
            blocked_reason: blocked_reason.map(str::to_string),
            blocking_dependencies: blocking_deps,
        };
        let json = serde_json::to_string(&record)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(MIGRATION_STATUS_TABLE)?;
            t.insert(key.as_str(), json.as_str())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Remove the status entry for a single file.
    ///
    /// Returns `true` when a record was found and deleted, `false` when the
    /// file had no status entry.
    pub fn delete_status(&self, project_id: &str, file_path: &str) -> anyhow::Result<bool> {
        let key = composite_key(project_id, file_path);
        let wtx = self.db.begin_write()?;
        let removed;
        {
            let mut t = wtx.open_table(MIGRATION_STATUS_TABLE)?;
            removed = t.remove(key.as_str())?.is_some();
        }
        wtx.commit()?;
        Ok(removed)
    }

    /// Remove **all** status entries for a project.
    ///
    /// Returns the number of entries deleted.
    pub fn clear_project(&self, project_id: &str) -> anyhow::Result<usize> {
        let prefix = format!("{project_id}\0");
        let wtx = self.db.begin_write()?;
        let mut count = 0usize;
        {
            let mut t = wtx.open_table(MIGRATION_STATUS_TABLE)?;
            // Collect keys to avoid mutating the table while iterating.
            let mut to_delete: Vec<String> = Vec::new();
            for r in t.iter()? {
                let (k, _v) = r?;
                if k.value().starts_with(prefix.as_str()) {
                    to_delete.push(k.value().to_string());
                }
            }
            for k in &to_delete {
                t.remove(k.as_str())?;
                count += 1;
            }
        }
        wtx.commit()?;
        Ok(count)
    }

    // ── Reads ──────────────────────────────────────────────────────────────

    /// Retrieve the status for a single file.  Returns `None` if the file has
    /// never been registered.
    pub fn get_status(
        &self,
        project_id: &str,
        file_path: &str,
    ) -> anyhow::Result<Option<FileStatus>> {
        let key = composite_key(project_id, file_path);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(MIGRATION_STATUS_TABLE)?;
        match t.get(key.as_str())? {
            Some(v) => {
                let fs: FileStatus = serde_json::from_str(v.value())?;
                Ok(Some(fs))
            }
            None => Ok(None),
        }
    }

    /// List all files for a project, optionally filtering by status.
    pub fn list_files(
        &self,
        project_id: &str,
        status_filter: Option<MigrationStatus>,
    ) -> anyhow::Result<Vec<FileStatus>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(MIGRATION_STATUS_TABLE)?;
        let mut out: Vec<FileStatus> = Vec::new();
        for r in t.iter()? {
            let (k, v) = r?;
            if !k.value().starts_with(prefix.as_str()) {
                continue;
            }
            let fs: FileStatus = serde_json::from_str(v.value())?;
            if let Some(ref filter) = status_filter {
                if &fs.status != filter {
                    continue;
                }
            }
            out.push(fs);
        }
        Ok(out)
    }

    /// Compute an aggregate progress report for a project.
    pub fn get_progress(&self, project_id: &str) -> anyhow::Result<MigrationProgress> {
        let files = self.list_files(project_id, None)?;

        let total = files.len();
        let mut not_started = 0usize;
        let mut in_progress = 0usize;
        let mut migrated = 0usize;
        let mut verified = 0usize;
        let mut blocked = 0usize;

        // file_type → (total, completed)
        let mut type_map: HashMap<String, (usize, usize)> = HashMap::new();

        let mut blocked_items: Vec<BlockedItem> = Vec::new();

        // Collect all files for sorting.
        let mut all_files: Vec<&FileStatus> = files.iter().collect();

        for fs in &files {
            match fs.status {
                MigrationStatus::NotStarted => not_started += 1,
                MigrationStatus::InProgress => in_progress += 1,
                MigrationStatus::Migrated => migrated += 1,
                MigrationStatus::Verified => verified += 1,
                MigrationStatus::Blocked => {
                    blocked += 1;
                    blocked_items.push(BlockedItem {
                        file_path: fs.file_path.clone(),
                        reason: fs
                            .blocked_reason
                            .clone()
                            .unwrap_or_else(|| "No reason specified".to_string()),
                        blocking_deps: fs.blocking_dependencies.clone(),
                    });
                }
            }

            let ext = file_extension(&fs.file_path);
            let entry = type_map.entry(ext).or_insert((0, 0));
            entry.0 += 1;
            if fs.status.is_complete() {
                entry.1 += 1;
            }
        }

        let completion_pct = if total == 0 {
            0.0
        } else {
            (migrated + verified) as f64 / total as f64 * 100.0
        };

        let by_file_type: HashMap<String, TypeProgress> = type_map
            .into_iter()
            .map(|(ext, (tot, comp))| {
                let pct = if tot == 0 {
                    0.0
                } else {
                    comp as f64 / tot as f64 * 100.0
                };
                (
                    ext,
                    TypeProgress {
                        total: tot,
                        completed: comp,
                        pct,
                    },
                )
            })
            .collect();

        // Recently updated: sort by last_updated descending, take 10.
        all_files.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
        let recently_updated: Vec<FileStatusSummary> = all_files
            .iter()
            .take(10)
            .map(|fs| FileStatusSummary {
                file_path: fs.file_path.clone(),
                status: fs.status.label().to_string(),
                notes: fs.notes.clone(),
            })
            .collect();

        // Suggested next: NotStarted files sorted by risk_score ascending
        // (None treated as 0 — lowest risk = easiest), take 5.
        let mut not_started_files: Vec<&FileStatus> = all_files
            .iter()
            .filter(|fs| fs.status == MigrationStatus::NotStarted)
            .copied()
            .collect();
        not_started_files.sort_by_key(|fs| fs.risk_score.unwrap_or(0));
        let suggested_next: Vec<String> = not_started_files
            .iter()
            .take(5)
            .map(|fs| fs.file_path.clone())
            .collect();

        Ok(MigrationProgress {
            project_id: project_id.to_string(),
            total_files: total,
            not_started,
            in_progress,
            migrated,
            verified,
            blocked,
            completion_pct,
            by_file_type,
            blocked_items,
            recently_updated,
            suggested_next,
        })
    }
}

// ─── Formatting ──────────────────────────────────────────────────────────────

/// Render a [`MigrationProgress`] as a human-readable Markdown report.
pub fn format_progress(report: &MigrationProgress) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let _ = writeln!(out, "# Migration Progress — `{}`\n", report.project_id);

    // ── Summary bar ──
    let bar = progress_bar(report.completion_pct, 30);
    let _ = writeln!(
        out,
        "**Overall**: {bar}  {:.1}% complete ({}/{} files)\n",
        report.completion_pct,
        report.migrated + report.verified,
        report.total_files,
    );

    // ── Status counts ──
    let _ = writeln!(out, "| Status | Count |");
    let _ = writeln!(out, "|--------|-------|");
    let _ = writeln!(out, "| Not Started | {} |", report.not_started);
    let _ = writeln!(out, "| In Progress | {} |", report.in_progress);
    let _ = writeln!(out, "| Migrated    | {} |", report.migrated);
    let _ = writeln!(out, "| Verified    | {} |", report.verified);
    let _ = writeln!(out, "| Blocked     | {} |\n", report.blocked);

    // ── By file type ──
    if !report.by_file_type.is_empty() {
        let _ = writeln!(out, "## By File Type\n");
        let _ = writeln!(out, "| Extension | Total | Done | % |");
        let _ = writeln!(out, "|-----------|-------|------|---|");
        let mut ext_list: Vec<(&String, &TypeProgress)> = report.by_file_type.iter().collect();
        ext_list.sort_by(|a, b| b.1.total.cmp(&a.1.total));
        for (ext, tp) in &ext_list {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {:.1}% |",
                ext, tp.total, tp.completed, tp.pct
            );
        }
        out.push('\n');
    }

    // ── Blocked items ──
    if !report.blocked_items.is_empty() {
        let _ = writeln!(out, "## Blocked Files\n");
        for item in &report.blocked_items {
            let _ = writeln!(out, "- **`{}`**: {}", item.file_path, item.reason);
            if !item.blocking_deps.is_empty() {
                let _ = writeln!(out, "  Waiting on: {}", item.blocking_deps.join(", "));
            }
        }
        out.push('\n');
    }

    // ── Suggested next ──
    if !report.suggested_next.is_empty() {
        let _ = writeln!(out, "## Suggested Next Files\n");
        for (i, path) in report.suggested_next.iter().enumerate() {
            let _ = writeln!(out, "{}. `{}`", i + 1, path);
        }
        out.push('\n');
    }

    // ── Recently updated ──
    if !report.recently_updated.is_empty() {
        let _ = writeln!(out, "## Recently Updated\n");
        let _ = writeln!(out, "| File | Status | Notes |");
        let _ = writeln!(out, "|------|--------|-------|");
        for s in &report.recently_updated {
            let notes = if s.notes.len() > 60 {
                format!("{}…", &s.notes[..60])
            } else {
                s.notes.clone()
            };
            let _ = writeln!(out, "| `{}` | {} | {} |", s.file_path, s.status, notes);
        }
    }

    out
}

// ─── Private helpers ─────────────────────────────────────────────────────────

/// Build the composite Redb key for a (project, file) pair.
#[inline]
fn composite_key(project_id: &str, file_path: &str) -> String {
    format!("{project_id}\0{file_path}")
}

/// Return the current time as epoch milliseconds.
#[inline]
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Extract the lowercase file extension from a path, or `"(none)"` when there
/// is no extension.
fn file_extension(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_else(|| "(none)".to_string())
}

/// Render an ASCII progress bar of a given width.
fn progress_bar(pct: f64, width: usize) -> String {
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(empty))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_store() -> (MigrationProgressStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("migration_progress.redb");
        let store = MigrationProgressStore::open(&db_path).unwrap();
        (store, dir)
    }

    // ── Test 1: Open a new database ──────────────────────────────────────────

    #[test]
    fn open_new_database() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("migration_progress.redb");
        assert!(!db_path.exists());
        let _store = MigrationProgressStore::open(&db_path).unwrap();
        assert!(db_path.exists(), "database file should be created on open");
    }

    // ── Test 2: Update and get status ────────────────────────────────────────

    #[test]
    fn update_and_get_status() {
        let (store, _dir) = make_store();

        store
            .update_status(
                "proj1",
                "src/Pages/Home.aspx",
                MigrationStatus::InProgress,
                "Started Blazor conversion",
                Some(30),
                None,
                vec![],
            )
            .unwrap();

        let fs = store
            .get_status("proj1", "src/Pages/Home.aspx")
            .unwrap()
            .expect("should find the status we just inserted");

        assert_eq!(fs.file_path, "src/Pages/Home.aspx");
        assert_eq!(fs.status, MigrationStatus::InProgress);
        assert_eq!(fs.notes, "Started Blazor conversion");
        assert_eq!(fs.risk_score, Some(30));
        assert!(fs.blocked_reason.is_none());
        assert!(fs.blocking_dependencies.is_empty());
        assert!(fs.last_updated > 0);
    }

    // ── Test 3: Progress aggregation ────────────────────────────────────────

    #[test]
    fn progress_aggregation() {
        let (store, _dir) = make_store();

        let files = vec![
            ("f1.aspx", MigrationStatus::Migrated),
            ("f2.aspx", MigrationStatus::Verified),
            ("f3.aspx", MigrationStatus::InProgress),
            ("f4.aspx", MigrationStatus::NotStarted),
            ("f5.aspx", MigrationStatus::NotStarted),
        ];

        for (path, status) in files {
            store
                .update_status("proj1", path, status, "", None, None, vec![])
                .unwrap();
        }

        let progress = store.get_progress("proj1").unwrap();
        assert_eq!(progress.total_files, 5);
        assert_eq!(progress.migrated, 1);
        assert_eq!(progress.verified, 1);
        assert_eq!(progress.in_progress, 1);
        assert_eq!(progress.not_started, 2);
        assert_eq!(progress.blocked, 0);
        // (1 + 1) / 5 * 100 = 40.0
        assert!((progress.completion_pct - 40.0).abs() < 0.01);
    }

    // ── Test 4: File type breakdown ──────────────────────────────────────────

    #[test]
    fn file_type_breakdown() {
        let (store, _dir) = make_store();

        let files = vec![
            ("Page.aspx", MigrationStatus::Migrated),
            ("Control.ascx", MigrationStatus::NotStarted),
            ("Service.asmx", MigrationStatus::Migrated),
            ("Helper.vb", MigrationStatus::Verified),
            ("Other.vb", MigrationStatus::NotStarted),
        ];

        for (path, status) in files {
            store
                .update_status("proj-types", path, status, "", None, None, vec![])
                .unwrap();
        }

        let progress = store.get_progress("proj-types").unwrap();

        let aspx = progress
            .by_file_type
            .get(".aspx")
            .expect(".aspx type present");
        assert_eq!(aspx.total, 1);
        assert_eq!(aspx.completed, 1);

        let vb = progress.by_file_type.get(".vb").expect(".vb type present");
        assert_eq!(vb.total, 2);
        assert_eq!(vb.completed, 1);
        assert!((vb.pct - 50.0).abs() < 0.01);

        let ascx = progress
            .by_file_type
            .get(".ascx")
            .expect(".ascx type present");
        assert_eq!(ascx.total, 1);
        assert_eq!(ascx.completed, 0);
    }

    // ── Test 5: Blocked items ────────────────────────────────────────────────

    #[test]
    fn blocked_items_appear_in_progress() {
        let (store, _dir) = make_store();

        store
            .update_status(
                "proj-blocked",
                "src/Admin.aspx",
                MigrationStatus::Blocked,
                "Waiting for auth module",
                Some(80),
                Some("AuthModule not migrated"),
                vec!["src/AuthModule.cs".to_string()],
            )
            .unwrap();

        store
            .update_status(
                "proj-blocked",
                "src/Index.aspx",
                MigrationStatus::NotStarted,
                "",
                Some(10),
                None,
                vec![],
            )
            .unwrap();

        let progress = store.get_progress("proj-blocked").unwrap();
        assert_eq!(progress.blocked, 1);
        assert_eq!(progress.blocked_items.len(), 1);

        let item = &progress.blocked_items[0];
        assert_eq!(item.file_path, "src/Admin.aspx");
        assert_eq!(item.reason, "AuthModule not migrated");
        assert_eq!(item.blocking_deps, vec!["src/AuthModule.cs".to_string()]);
    }

    // ── Test 6: Status filter ────────────────────────────────────────────────

    #[test]
    fn list_files_with_status_filter() {
        let (store, _dir) = make_store();

        let files = vec![
            ("a.aspx", MigrationStatus::NotStarted),
            ("b.aspx", MigrationStatus::InProgress),
            ("c.aspx", MigrationStatus::Migrated),
            ("d.aspx", MigrationStatus::NotStarted),
        ];

        for (path, status) in files {
            store
                .update_status("proj-filter", path, status, "", None, None, vec![])
                .unwrap();
        }

        let not_started = store
            .list_files("proj-filter", Some(MigrationStatus::NotStarted))
            .unwrap();
        assert_eq!(not_started.len(), 2);
        assert!(
            not_started
                .iter()
                .all(|f| f.status == MigrationStatus::NotStarted)
        );

        let migrated = store
            .list_files("proj-filter", Some(MigrationStatus::Migrated))
            .unwrap();
        assert_eq!(migrated.len(), 1);
        assert_eq!(migrated[0].file_path, "c.aspx");

        let all = store.list_files("proj-filter", None).unwrap();
        assert_eq!(all.len(), 4);
    }

    // ── Test 7: Delete and clear ─────────────────────────────────────────────

    #[test]
    fn delete_and_clear_project() {
        let (store, _dir) = make_store();

        for i in 0..4 {
            store
                .update_status(
                    "proj-del",
                    &format!("file{i}.aspx"),
                    MigrationStatus::NotStarted,
                    "",
                    None,
                    None,
                    vec![],
                )
                .unwrap();
        }

        // Delete a single file.
        let deleted = store.delete_status("proj-del", "file2.aspx").unwrap();
        assert!(deleted, "delete should return true when entry existed");
        let not_found = store.delete_status("proj-del", "file2.aspx").unwrap();
        assert!(!not_found, "second delete should return false");

        let remaining = store.list_files("proj-del", None).unwrap();
        assert_eq!(remaining.len(), 3);

        // Clear all.
        let cleared = store.clear_project("proj-del").unwrap();
        assert_eq!(cleared, 3, "should have cleared remaining 3 files");

        let after_clear = store.list_files("proj-del", None).unwrap();
        assert!(after_clear.is_empty());
    }

    // ── Test 8: Suggested next files ordered by risk_score ──────────────────

    #[test]
    fn suggested_next_ordered_by_risk_score() {
        let (store, _dir) = make_store();

        // Insert 8 files: 7 NotStarted with varying risk scores + 1 Migrated.
        let files = vec![
            ("high_risk.aspx", MigrationStatus::NotStarted, Some(90u8)),
            ("low_risk.aspx", MigrationStatus::NotStarted, Some(5)),
            ("medium_risk.aspx", MigrationStatus::NotStarted, Some(50)),
            ("no_risk.aspx", MigrationStatus::NotStarted, None),
            ("also_low.aspx", MigrationStatus::NotStarted, Some(10)),
            ("already_done.aspx", MigrationStatus::Migrated, Some(1)),
            ("another_low.aspx", MigrationStatus::NotStarted, Some(15)),
            ("trivial.aspx", MigrationStatus::NotStarted, Some(3)),
        ];

        for (path, status, risk) in files {
            store
                .update_status("proj-suggest", path, status, "", risk, None, vec![])
                .unwrap();
        }

        let progress = store.get_progress("proj-suggest").unwrap();

        // Should contain up to 5 NotStarted files, lowest risk first.
        assert_eq!(progress.suggested_next.len(), 5);

        // The first suggested file must be the one with risk_score == None
        // (treated as 0) or the one with risk_score == 5 — both are equal
        // candidates; ensure risk_score=5 and None (0) appear before 50+.
        let first_risk_scores: Vec<Option<u8>> = progress
            .suggested_next
            .iter()
            .map(|path| {
                store
                    .get_status("proj-suggest", path)
                    .unwrap()
                    .unwrap()
                    .risk_score
            })
            .collect();

        // All suggested should have risk <= 15 (the 5 lowest: None/0, 5, 10, 15, and next lowest is 50).
        // Verify the highest risk in suggestions is at most 15.
        let max_risk = first_risk_scores
            .iter()
            .map(|r| r.unwrap_or(0))
            .max()
            .unwrap_or(0);
        assert!(
            max_risk <= 15,
            "suggested files should be low-risk; got max risk {max_risk}"
        );

        // The `already_done.aspx` (Migrated) must never appear.
        assert!(
            !progress
                .suggested_next
                .contains(&"already_done.aspx".to_string()),
            "migrated files must not appear in suggested_next"
        );
    }

    // ── Test 9: Project isolation ────────────────────────────────────────────

    #[test]
    fn different_projects_are_isolated() {
        let (store, _dir) = make_store();

        store
            .update_status(
                "alpha",
                "shared_name.aspx",
                MigrationStatus::Migrated,
                "done",
                None,
                None,
                vec![],
            )
            .unwrap();
        store
            .update_status(
                "beta",
                "shared_name.aspx",
                MigrationStatus::NotStarted,
                "",
                None,
                None,
                vec![],
            )
            .unwrap();

        let alpha_status = store
            .get_status("alpha", "shared_name.aspx")
            .unwrap()
            .unwrap();
        assert_eq!(alpha_status.status, MigrationStatus::Migrated);

        let beta_status = store
            .get_status("beta", "shared_name.aspx")
            .unwrap()
            .unwrap();
        assert_eq!(beta_status.status, MigrationStatus::NotStarted);

        // Clearing alpha must not touch beta.
        store.clear_project("alpha").unwrap();
        let beta_after = store.get_status("beta", "shared_name.aspx").unwrap();
        assert!(beta_after.is_some(), "beta entry must survive alpha clear");
    }

    // ── Test 10: format_progress produces non-empty Markdown ────────────────

    #[test]
    fn format_progress_produces_markdown() {
        let (store, _dir) = make_store();

        store
            .update_status(
                "proj-fmt",
                "Home.aspx",
                MigrationStatus::Migrated,
                "done",
                Some(10),
                None,
                vec![],
            )
            .unwrap();
        store
            .update_status(
                "proj-fmt",
                "Admin.aspx",
                MigrationStatus::Blocked,
                "waiting",
                Some(80),
                Some("Needs auth"),
                vec!["Auth.cs".to_string()],
            )
            .unwrap();
        store
            .update_status(
                "proj-fmt",
                "Index.aspx",
                MigrationStatus::NotStarted,
                "",
                Some(5),
                None,
                vec![],
            )
            .unwrap();

        let progress = store.get_progress("proj-fmt").unwrap();
        let report = format_progress(&progress);

        assert!(
            report.contains("proj-fmt"),
            "report must contain project id"
        );
        assert!(
            report.contains("40.0%") || report.contains("33.3%"),
            "report must contain completion pct"
        );
        assert!(
            report.contains("Blocked Files"),
            "report must have blocked section"
        );
        assert!(
            report.contains("Suggested Next"),
            "report must have suggested next section"
        );
        assert!(
            report.contains(".aspx"),
            "report must contain file type breakdown"
        );
    }
}
