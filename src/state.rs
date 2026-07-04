use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::models::{DocumentPayload, JobStage, Repository};

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StatusSummary {
    pub selected_repositories: u64,
    pub total_documents: u64,
    pub completed_documents: u64,
    pub failed_documents: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct DocumentStateUpdate<'a> {
    pub stage: JobStage,
    pub hash: Option<&'a str>,
    pub local_path: Option<&'a str>,
    pub error: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct FileIntegrityIssue {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct DocumentResumeState {
    pub stage: JobStage,
    pub remote_updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredBitmap {
    pub encoding: String,
    pub cardinality: usize,
    pub blob: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct StoredResumeSnapshot {
    pub doc_count: usize,
    pub bitmaps: BTreeMap<String, StoredBitmap>,
}

#[derive(Debug, Clone)]
pub struct ResumeItemWrite {
    pub ordinal: usize,
    pub doc_id: String,
    pub slug: String,
    pub title: String,
    pub remote_updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResumeBitmapWrite {
    pub name: String,
    pub encoding: String,
    pub cardinality: usize,
    pub blob: Vec<u8>,
}

impl DocumentStateUpdate<'_> {
    pub fn stage(stage: JobStage) -> Self {
        Self {
            stage,
            hash: None,
            local_path: None,
            error: None,
        }
    }
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = Self { path };
        store.with_conn(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS selections (
                    host TEXT NOT NULL,
                    repo_id TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    repo_name TEXT NOT NULL,
                    selected INTEGER NOT NULL DEFAULT 1,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY(host, repo_id)
                 );
                 CREATE TABLE IF NOT EXISTS documents (
                    host TEXT NOT NULL,
                    repo_id TEXT NOT NULL,
                    doc_id TEXT NOT NULL,
                    slug TEXT NOT NULL,
                    title TEXT NOT NULL,
                    remote_updated_at TEXT,
                    content_hash TEXT,
                    stage TEXT NOT NULL,
                    local_path TEXT,
                    error TEXT,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY(host, repo_id, doc_id)
                 );
                 CREATE TABLE IF NOT EXISTS request_log (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    host TEXT NOT NULL,
                    bucket TEXT NOT NULL,
                    requested_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS request_log_window
                    ON request_log(host, bucket, requested_at);
                 CREATE TABLE IF NOT EXISTS remote_deletions (
                    host TEXT NOT NULL,
                    repo_id TEXT NOT NULL,
                    doc_id TEXT NOT NULL,
                    detected_at INTEGER NOT NULL,
                    PRIMARY KEY(host, repo_id, doc_id)
                 );
                 CREATE TABLE IF NOT EXISTS archived_files (
                    host TEXT NOT NULL,
                    repo_id TEXT NOT NULL,
                    doc_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    sha256 TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY(host, path)
                 );
                 CREATE TABLE IF NOT EXISTS resume_snapshots (
                    host TEXT NOT NULL,
                    repo_id TEXT NOT NULL,
                    snapshot_hash TEXT NOT NULL,
                    doc_count INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY(host, repo_id, snapshot_hash)
                 );
                 CREATE TABLE IF NOT EXISTS resume_items (
                    host TEXT NOT NULL,
                    repo_id TEXT NOT NULL,
                    snapshot_hash TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    doc_id TEXT NOT NULL,
                    slug TEXT NOT NULL,
                    title TEXT NOT NULL,
                    remote_updated_at TEXT,
                    PRIMARY KEY(host, repo_id, snapshot_hash, ordinal)
                 );
                 CREATE INDEX IF NOT EXISTS resume_items_doc
                    ON resume_items(host, repo_id, snapshot_hash, doc_id);
                 CREATE TABLE IF NOT EXISTS resume_bitmaps (
                    host TEXT NOT NULL,
                    repo_id TEXT NOT NULL,
                    snapshot_hash TEXT NOT NULL,
                    name TEXT NOT NULL,
                    encoding TEXT NOT NULL,
                    cardinality INTEGER NOT NULL,
                    blob BLOB NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY(host, repo_id, snapshot_hash, name)
                 );",
            )?;
            Ok(())
        })?;
        Ok(store)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("打开状态库失败: {}", self.path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        f(&conn)
    }

    pub fn set_selection(&self, host: &str, repo: &Repository, selected: bool) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO selections(host, repo_id, namespace, repo_name, selected, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(host, repo_id) DO UPDATE SET namespace=excluded.namespace,
                    repo_name=excluded.repo_name, selected=excluded.selected, updated_at=excluded.updated_at",
                params![host, repo.id, repo.namespace, repo.name, selected as i64, now_epoch()],
            )?;
            Ok(())
        })
    }

    pub fn selected_repo_ids(&self, host: &str) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT repo_id FROM selections WHERE host=?1 AND selected=1 ORDER BY repo_name",
            )?;
            let rows = stmt.query_map([host], |row| row.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn document_stage(
        &self,
        host: &str,
        repo_id: &str,
        doc_id: &str,
    ) -> Result<Option<JobStage>> {
        self.with_conn(|conn| {
            let value: Option<String> = conn
                .query_row(
                    "SELECT stage FROM documents WHERE host=?1 AND repo_id=?2 AND doc_id=?3",
                    params![host, repo_id, doc_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value.map(|v| JobStage::from_db(&v)))
        })
    }

    pub fn document_remote_updated(
        &self,
        host: &str,
        repo_id: &str,
        doc_id: &str,
    ) -> Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT remote_updated_at FROM documents WHERE host=?1 AND repo_id=?2 AND doc_id=?3",
                    params![host, repo_id, doc_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten())
        })
    }

    pub fn document_states_for_repo(
        &self,
        host: &str,
        repo_id: &str,
    ) -> Result<HashMap<String, DocumentResumeState>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT doc_id, stage, remote_updated_at
                 FROM documents
                 WHERE host=?1 AND repo_id=?2",
            )?;
            let rows = stmt.query_map(params![host, repo_id], |row| {
                let doc_id: String = row.get(0)?;
                let stage: String = row.get(1)?;
                let remote_updated_at: Option<String> = row.get(2)?;
                Ok((
                    doc_id,
                    DocumentResumeState {
                        stage: JobStage::from_db(&stage),
                        remote_updated_at,
                    },
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
        })
    }

    pub fn load_resume_bitmaps(
        &self,
        host: &str,
        repo_id: &str,
        snapshot_hash: &str,
    ) -> Result<Option<StoredResumeSnapshot>> {
        self.with_conn(|conn| {
            let doc_count: Option<i64> = conn
                .query_row(
                    "SELECT doc_count FROM resume_snapshots
                     WHERE host=?1 AND repo_id=?2 AND snapshot_hash=?3",
                    params![host, repo_id, snapshot_hash],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(doc_count) = doc_count else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(
                "SELECT name, encoding, cardinality, blob
                 FROM resume_bitmaps
                 WHERE host=?1 AND repo_id=?2 AND snapshot_hash=?3
                 ORDER BY name",
            )?;
            let rows = stmt.query_map(params![host, repo_id, snapshot_hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    StoredBitmap {
                        encoding: row.get(1)?,
                        cardinality: row.get::<_, i64>(2)? as usize,
                        blob: row.get(3)?,
                    },
                ))
            })?;
            Ok(Some(StoredResumeSnapshot {
                doc_count: doc_count as usize,
                bitmaps: rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?,
            }))
        })
    }

    pub fn save_resume_bitmaps(
        &self,
        host: &str,
        repo_id: &str,
        snapshot_hash: &str,
        doc_count: usize,
        items: &[ResumeItemWrite],
        bitmaps: &[ResumeBitmapWrite],
    ) -> Result<()> {
        let mut conn = Connection::open(&self.path)
            .with_context(|| format!("打开状态库失败: {}", self.path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO resume_snapshots(host,repo_id,snapshot_hash,doc_count,updated_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(host,repo_id,snapshot_hash) DO UPDATE SET
                doc_count=excluded.doc_count, updated_at=excluded.updated_at",
            params![host, repo_id, snapshot_hash, doc_count as i64, now_epoch()],
        )?;
        let existing_items: i64 = tx.query_row(
            "SELECT COUNT(*) FROM resume_items WHERE host=?1 AND repo_id=?2 AND snapshot_hash=?3",
            params![host, repo_id, snapshot_hash],
            |row| row.get(0),
        )?;
        if existing_items as usize != doc_count {
            tx.execute(
                "DELETE FROM resume_items WHERE host=?1 AND repo_id=?2 AND snapshot_hash=?3",
                params![host, repo_id, snapshot_hash],
            )?;
            for item in items {
                tx.execute(
                    "INSERT INTO resume_items(host,repo_id,snapshot_hash,ordinal,doc_id,slug,title,remote_updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        host,
                        repo_id,
                        snapshot_hash,
                        item.ordinal as i64,
                        item.doc_id.as_str(),
                        item.slug.as_str(),
                        item.title.as_str(),
                        item.remote_updated_at.as_deref()
                    ],
                )?;
            }
        }
        tx.execute(
            "DELETE FROM resume_bitmaps WHERE host=?1 AND repo_id=?2 AND snapshot_hash=?3",
            params![host, repo_id, snapshot_hash],
        )?;
        for bitmap in bitmaps {
            tx.execute(
                "INSERT INTO resume_bitmaps(host,repo_id,snapshot_hash,name,encoding,cardinality,blob,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    host,
                    repo_id,
                    snapshot_hash,
                    bitmap.name.as_str(),
                    bitmap.encoding.as_str(),
                    bitmap.cardinality as i64,
                    bitmap.blob.as_slice(),
                    now_epoch()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_document(
        &self,
        host: &str,
        repo_id: &str,
        doc: &DocumentPayload,
        update: DocumentStateUpdate<'_>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO documents(host, repo_id, doc_id, slug, title, remote_updated_at, content_hash, stage, local_path, error, updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(host,repo_id,doc_id) DO UPDATE SET slug=excluded.slug,title=excluded.title,
                    remote_updated_at=excluded.remote_updated_at,content_hash=COALESCE(excluded.content_hash,documents.content_hash),
                    stage=excluded.stage,local_path=COALESCE(excluded.local_path,documents.local_path),error=excluded.error,updated_at=excluded.updated_at",
                params![host, repo_id, doc.doc_id, doc.slug, doc.title, doc.content_updated_at.as_ref().or(doc.updated_at.as_ref()), update.hash,
                    update.stage.as_str(), update.local_path, update.error, now_epoch()],
            )?;
            Ok(())
        })
    }

    pub fn record_request(&self, host: &str, bucket: &str, timestamp_ms: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO request_log(host,bucket,requested_at) VALUES(?1,?2,?3)",
                params![host, bucket, timestamp_ms],
            )?;
            conn.execute(
                "DELETE FROM request_log WHERE requested_at < ?1",
                [timestamp_ms - 86_400_000],
            )?;
            Ok(())
        })
    }

    pub fn request_window(&self, host: &str, bucket: &str, since_ms: i64) -> Result<Vec<i64>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT requested_at FROM request_log WHERE host=?1 AND bucket=?2 AND requested_at>=?3 ORDER BY requested_at")?;
            let rows = stmt.query_map(params![host,bucket,since_ms], |row| row.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn status(&self, host: &str) -> Result<StatusSummary> {
        self.with_conn(|conn| {
            let selected_repositories: i64 = conn.query_row(
                "SELECT COUNT(*) FROM selections WHERE host=?1 AND selected=1",
                [host],
                |r| r.get(0),
            )?;
            let total_documents: i64 = conn.query_row(
                "SELECT COUNT(*) FROM documents WHERE host=?1",
                [host],
                |r| r.get(0),
            )?;
            let completed_documents: i64 = conn.query_row(
                "SELECT COUNT(*) FROM documents WHERE host=?1 AND stage='complete'",
                [host],
                |r| r.get(0),
            )?;
            let failed_documents: i64 = conn.query_row(
                "SELECT COUNT(*) FROM documents WHERE host=?1 AND stage='failed'",
                [host],
                |r| r.get(0),
            )?;
            Ok(StatusSummary {
                selected_repositories: selected_repositories as u64,
                total_documents: total_documents as u64,
                completed_documents: completed_documents as u64,
                failed_documents: failed_documents as u64,
            })
        })
    }

    pub fn reconcile_remote_documents(
        &self,
        host: &str,
        repo_id: &str,
        current_ids: &std::collections::HashSet<String>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT doc_id FROM documents WHERE host=?1 AND repo_id=?2 AND stage='complete'",
            )?;
            let known = stmt
                .query_map(params![host, repo_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for doc_id in known {
                if current_ids.contains(&doc_id) {
                    conn.execute(
                        "DELETE FROM remote_deletions WHERE host=?1 AND repo_id=?2 AND doc_id=?3",
                        params![host, repo_id, doc_id],
                    )?;
                } else {
                    conn.execute(
                        "INSERT INTO remote_deletions(host,repo_id,doc_id,detected_at) VALUES(?1,?2,?3,?4)
                         ON CONFLICT(host,repo_id,doc_id) DO NOTHING",
                        params![host, repo_id, doc_id, now_epoch()],
                    )?;
                }
            }
            Ok(())
        })
    }

    pub fn record_file(&self, record: &ArchivedFileRecord<'_>) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO archived_files(host,repo_id,doc_id,path,sha256,size,kind,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(host,path) DO UPDATE SET repo_id=excluded.repo_id,doc_id=excluded.doc_id,
                    sha256=excluded.sha256,size=excluded.size,kind=excluded.kind,updated_at=excluded.updated_at",
                params![record.host, record.repo_id, record.doc_id, record.path.to_string_lossy(),
                    record.sha256, record.size as i64, record.kind, now_epoch()],
            )?;
            Ok(())
        })
    }

    pub fn verify_files(&self, host: &str) -> Result<Vec<FileIntegrityIssue>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path,sha256,size FROM archived_files WHERE host=?1 ORDER BY path",
            )?;
            let rows = stmt.query_map([host], |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            let mut issues = Vec::new();
            for row in rows {
                let (path, expected_hash, expected_size) = row?;
                match fs::read(&path) {
                    Ok(bytes) => {
                        let actual_hash = format!("{:x}", Sha256::digest(&bytes));
                        if bytes.len() as i64 != expected_size || actual_hash != expected_hash {
                            issues.push(FileIntegrityIssue {
                                path,
                                reason: "大小或 SHA-256 不匹配".into(),
                            });
                        }
                    }
                    Err(error) => issues.push(FileIntegrityIssue {
                        path,
                        reason: format!("无法读取: {error}"),
                    }),
                }
            }
            Ok(issues)
        })
    }
}

pub struct ArchivedFileRecord<'a> {
    pub host: &'a str,
    pub repo_id: &'a str,
    pub doc_id: &'a str,
    pub path: &'a Path,
    pub sha256: &'a str,
    pub size: u64,
    pub kind: &'a str,
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_are_isolated_by_host() {
        let dir = std::env::temp_dir().join(format!("yuque-backup-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = StateStore::open(dir.join("state.sqlite3")).unwrap();
        let repo = Repository {
            id: "1".into(),
            slug: "repo".into(),
            name: "Repo".into(),
            namespace: "team/repo".into(),
            owner_login: "team".into(),
            description: None,
            items_count: None,
            selected: true,
        };
        state
            .set_selection("https://a.yuque.com", &repo, true)
            .unwrap();
        assert_eq!(
            state.selected_repo_ids("https://a.yuque.com").unwrap(),
            vec!["1"]
        );
        assert!(
            state
                .selected_repo_ids("https://b.yuque.com")
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detects_file_hash_mismatch() {
        let dir =
            std::env::temp_dir().join(format!("yuque-integrity-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = StateStore::open(dir.join("state.sqlite3")).unwrap();
        let path = dir.join("doc.md");
        std::fs::write(&path, b"original").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"original"));
        state
            .record_file(&ArchivedFileRecord {
                host: "https://a.yuque.com",
                repo_id: "1",
                doc_id: "2",
                path: &path,
                sha256: &hash,
                size: 8,
                kind: "markdown",
            })
            .unwrap();
        assert!(
            state
                .verify_files("https://a.yuque.com")
                .unwrap()
                .is_empty()
        );
        std::fs::write(&path, b"changed").unwrap();
        assert_eq!(state.verify_files("https://a.yuque.com").unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loads_document_states_for_repo_in_one_query() {
        let dir =
            std::env::temp_dir().join(format!("yuque-resume-state-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = StateStore::open(dir.join("state.sqlite3")).unwrap();
        let doc = DocumentPayload {
            doc_id: "doc-1".into(),
            slug: "doc".into(),
            title: "Doc".into(),
            updated_at: Some("2026-01-01T00:00:00Z".into()),
            content_updated_at: Some("2026-01-02T00:00:00Z".into()),
            markdown: None,
            body_html: None,
            body_lake: None,
            sheet: None,
            raw: serde_json::Value::Null,
            diagram_raw: None,
        };
        state
            .upsert_document(
                "https://a.yuque.com",
                "repo-1",
                &doc,
                DocumentStateUpdate::stage(JobStage::Complete),
            )
            .unwrap();
        let states = state
            .document_states_for_repo("https://a.yuque.com", "repo-1")
            .unwrap();
        let loaded = states.get("doc-1").unwrap();
        assert_eq!(loaded.stage, JobStage::Complete);
        assert_eq!(
            loaded.remote_updated_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
