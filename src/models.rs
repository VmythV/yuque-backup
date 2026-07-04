use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TeamKind {
    Personal,
    Group,
    Collaboration,
}

impl TeamKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Group => "group",
            Self::Collaboration => "collaboration",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub login: String,
    pub name: String,
    pub kind: TeamKind,
    pub repositories: Vec<Repository>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub namespace: String,
    pub owner_login: String,
    pub description: Option<String>,
    pub items_count: Option<u64>,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TocItem {
    pub id: String,
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub title: String,
    pub slug: Option<String>,
    pub item_type: String,
    pub visible: bool,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub repository: Repository,
    pub toc: Vec<TocItem>,
    pub raw_app_data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentPayload {
    pub doc_id: String,
    pub slug: String,
    pub title: String,
    pub updated_at: Option<String>,
    pub content_updated_at: Option<String>,
    pub markdown: Option<String>,
    pub body_html: Option<String>,
    pub body_lake: Option<String>,
    pub sheet: Option<String>,
    pub raw: serde_json::Value,
    pub diagram_raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteDocumentMeta {
    pub id: String,
    pub slug: String,
    pub updated_at: Option<String>,
    pub content_updated_at: Option<String>,
}

impl RemoteDocumentMeta {
    pub fn effective_updated_at(&self) -> Option<&str> {
        self.content_updated_at
            .as_deref()
            .or(self.updated_at.as_deref())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobStage {
    Pending,
    MetadataSaved,
    ContentSaved,
    AssetsDownloading,
    Rendered,
    Verified,
    Complete,
    Failed,
}

impl JobStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::MetadataSaved => "metadata_saved",
            Self::ContentSaved => "content_saved",
            Self::AssetsDownloading => "assets_downloading",
            Self::Rendered => "rendered",
            Self::Verified => "verified",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "metadata_saved" => Self::MetadataSaved,
            "content_saved" => Self::ContentSaved,
            "assets_downloading" => Self::AssetsDownloading,
            "rendered" => Self::Rendered,
            "verified" => Self::Verified,
            "complete" => Self::Complete,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncProgress {
    pub team: String,
    pub repository: String,
    pub document: String,
    pub stage: String,
    pub repository_index: usize,
    pub repository_total: usize,
    pub repository_completed: usize,
    pub completed: usize,
    pub total: usize,
    pub failed: usize,
    pub skipped: usize,
    pub downloaded: usize,
    pub overall_completed: usize,
    pub overall_total: usize,
    pub overall_failed: usize,
    pub overall_skipped: usize,
    pub overall_downloaded: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RateLimitProgress {
    pub bucket: String,
    pub reason: RateLimitReason,
    pub wait_until_ms: i64,
    pub wait_seconds: u64,
    pub used: Option<usize>,
    pub usable: Option<usize>,
    pub endpoint: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum RateLimitReason {
    HourlyLimit,
    ServerRetryAfter,
}

impl RateLimitReason {
    pub fn label(&self) -> &'static str {
        match self {
            Self::HourlyLimit => "本地小时限流",
            Self::ServerRetryAfter => "服务端 429",
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SyncSummary {
    pub repository_total: usize,
    pub repository_completed: usize,
    pub document_total: usize,
    pub document_completed: usize,
    pub document_downloaded: usize,
    pub document_skipped: usize,
    pub document_failed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub enum SyncEvent {
    Progress(SyncProgress),
    RateLimit(RateLimitProgress),
    Warning { message: String },
    Finished { summary: SyncSummary },
    Failed { message: String },
}
