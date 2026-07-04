use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use futures::{StreamExt, stream};
use regex::Regex;
use reqwest::{
    Client, StatusCode,
    header::{COOKIE, RETRY_AFTER, USER_AGENT},
};
use sha2::{Digest, Sha256};

use crate::{
    archive::{Archive, atomic_write, content_hash, safe_component},
    config::AppConfig,
    models::{JobStage, Repository, SyncEvent, SyncProgress, SyncSummary, Team},
    provider::YuqueProvider,
    rate_limit::{PersistentRateLimiter, RateLimitCallback},
    resume::{ResumeDoc, ResumePlan},
    state::{ArchivedFileRecord, DocumentResumeState, DocumentStateUpdate, StateStore},
};

struct DownloadedAsset {
    path: PathBuf,
    hash: String,
    size: u64,
}

#[derive(Debug, Clone, Default)]
struct RepositorySyncStats {
    total: usize,
    completed: usize,
    failed: usize,
    skipped: usize,
    downloaded: usize,
}

pub struct Downloader<P: YuqueProvider> {
    config: AppConfig,
    provider: Arc<P>,
    state: StateStore,
    archive: Archive,
    asset_client: Client,
    asset_limiter: PersistentRateLimiter,
}

impl<P: YuqueProvider> Downloader<P> {
    pub fn new(
        config: AppConfig,
        provider: Arc<P>,
        state: StateStore,
        rate_limit_callback: Option<RateLimitCallback>,
    ) -> Result<Self> {
        let archive = Archive::new(config.output_dir.clone());
        let asset_limiter =
            PersistentRateLimiter::assets(config.host.clone(), &config.rate_limit, state.clone());
        asset_limiter.set_callback(rate_limit_callback);
        let asset_client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            config,
            provider,
            state,
            archive,
            asset_client,
            asset_limiter,
        })
    }

    pub async fn sync(
        &self,
        teams: &[Team],
        mut emit: impl FnMut(SyncEvent),
    ) -> Result<SyncSummary> {
        let selected_repositories = teams
            .iter()
            .flat_map(|team| team.repositories.iter())
            .filter(|repo| repo.selected)
            .count();
        let mut summary = SyncSummary {
            repository_total: selected_repositories,
            ..SyncSummary::default()
        };
        self.archive.save_manifest(&self.config.host, teams).await?;
        for team in teams {
            for repo in team.repositories.iter().filter(|r| r.selected) {
                self.sync_repository(
                    team,
                    repo,
                    summary.repository_completed + 1,
                    selected_repositories,
                    &mut summary,
                    &mut emit,
                )
                .await?;
                summary.repository_completed += 1;
            }
        }
        self.archive.save_manifest(&self.config.host, teams).await?;
        Ok(summary)
    }

    async fn sync_repository(
        &self,
        team: &Team,
        repo: &Repository,
        repository_index: usize,
        repository_total: usize,
        summary: &mut SyncSummary,
        emit: &mut impl FnMut(SyncEvent),
    ) -> Result<()> {
        let mut repo_stats = RepositorySyncStats::default();
        emit_progress(
            emit,
            ProgressContext {
                summary,
                team,
                repo,
                repository_index,
                repository_total,
                repo_stats: &repo_stats,
            },
            "",
            "获取目录",
            "获取知识库目录",
        );
        let snapshot = self
            .provider
            .repository_snapshot(repo)
            .await
            .with_context(|| format!("获取知识库目录失败: {}", repo.namespace))?;
        self.archive.save_repository(team, &snapshot).await?;
        let remote_index = match self.provider.document_index(repo).await {
            Ok(index) => index,
            Err(error) => {
                tracing::warn!(repo=%repo.namespace, %error, "无法获取更新时间索引，将仅按本地完成状态续传");
                emit(SyncEvent::Warning {
                    message: format!(
                        "{} / {}：无法获取更新时间索引，将仅按本地完成状态续传：{error}",
                        team.name, repo.name
                    ),
                });
                Default::default()
            }
        };
        let docs: Vec<_> = snapshot
            .toc
            .iter()
            .filter(|item| {
                item.visible
                    && item.slug.is_some()
                    && matches!(
                        item.item_type.to_ascii_uppercase().as_str(),
                        "DOC" | "TABLE" | "SHEET"
                    )
            })
            .collect();
        repo_stats.total = docs.len();
        summary.document_total += docs.len();
        emit_progress(
            emit,
            ProgressContext {
                summary,
                team,
                repo,
                repository_index,
                repository_total,
                repo_stats: &repo_stats,
            },
            "",
            "校验",
            "读取本地断点状态",
        );
        let local_states = self
            .state
            .document_states_for_repo(&self.config.host, &repo.id)?;
        let current_ids = docs.iter().map(|item| item.id.clone()).collect();
        self.state
            .reconcile_remote_documents(&self.config.host, &repo.id, &current_ids)?;

        let resume_docs = docs
            .iter()
            .map(|toc| ResumeDoc {
                doc_id: toc.id.clone(),
                slug: toc.slug.clone().unwrap_or_default(),
                title: toc.title.clone(),
                remote_updated_at: remote_index
                    .get(toc.slug.as_deref().unwrap_or_default())
                    .and_then(|meta| meta.effective_updated_at())
                    .map(str::to_owned),
            })
            .collect::<Vec<_>>();
        let mut resume_plan = ResumePlan::load_or_rebuild(
            &self.state,
            &self.config.host,
            &repo.id,
            resume_docs,
            &local_states,
        )?;
        let backfill_ordinals = self
            .diagram_backfill_ordinals(team, repo, &docs, &local_states)
            .await?;
        for ordinal in &backfill_ordinals {
            resume_plan.mark_pending(*ordinal);
        }
        let resume_stats = resume_plan.stats();
        repo_stats.completed = resume_plan.processed_count();
        repo_stats.skipped = resume_stats.skipped;
        repo_stats.downloaded = resume_stats.downloaded;
        repo_stats.failed = resume_stats.failed;
        summary.document_completed += repo_stats.completed;
        summary.document_skipped += repo_stats.skipped;
        summary.document_downloaded += repo_stats.downloaded;
        summary.document_failed += repo_stats.failed;
        let pending = repo_stats.total.saturating_sub(resume_stats.done);
        emit_progress(
            emit,
            ProgressContext {
                summary,
                team,
                repo,
                repository_index,
                repository_total,
                repo_stats: &repo_stats,
            },
            "",
            "恢复计划",
            &format!(
                "{}，待处理 {pending}，已跳过 {}",
                if resume_plan.loaded_from_cache {
                    "Bitmap 缓存命中"
                } else {
                    "Bitmap 已重建"
                },
                if backfill_ordinals.is_empty() {
                    repo_stats.skipped.to_string()
                } else {
                    format!(
                        "{}，diagram 回填 {}",
                        repo_stats.skipped,
                        backfill_ordinals.len()
                    )
                }
            ),
        );
        resume_plan.flush(&self.state, &self.config.host, &repo.id)?;

        let mut cursor = 0_usize;
        let mut processed_since_flush = 0_usize;
        while let Some(ordinal) = resume_plan.next_pending_from(cursor) {
            cursor = ordinal + 1;
            let Some(toc) = docs.get(ordinal).copied() else {
                continue;
            };
            emit_progress(
                emit,
                ProgressContext {
                    summary,
                    team,
                    repo,
                    repository_index,
                    repository_total,
                    repo_stats: &repo_stats,
                },
                &toc.title,
                "下载正文",
                "下载正文",
            );
            let result = async {
                let mut doc = self.provider.document(repo, toc).await?;
                self.state.upsert_document(
                    &self.config.host,
                    &repo.id,
                    &doc,
                    DocumentStateUpdate::stage(JobStage::MetadataSaved),
                )?;
                let raw_path = if self.config.sync.keep_raw {
                    let path = self.archive.save_document_raw(team, repo, &doc).await?;
                    self.record_existing_file(repo, &doc.doc_id, &path, "raw-json")
                        .await?;
                    Some(path)
                } else {
                    None
                };
                self.state.upsert_document(
                    &self.config.host,
                    &repo.id,
                    &doc,
                    DocumentStateUpdate::stage(JobStage::ContentSaved),
                )?;
                if doc.sheet.is_some() {
                    for path in self.archive.save_sheet(team, repo, &doc).await? {
                        self.record_existing_file(repo, &doc.doc_id, &path, "sheet-csv")
                            .await?;
                    }
                }
                if doc.diagram_raw.is_some() {
                    for path in self.archive.save_diagrams(team, repo, &doc).await? {
                        self.record_existing_file(repo, &doc.doc_id, &path, "diagram")
                            .await?;
                    }
                }

                if !self.config.sync.render_markdown {
                    let raw_bytes = serde_json::to_vec(&doc.raw)?;
                    let hash = content_hash(&raw_bytes);
                    self.state.upsert_document(
                        &self.config.host,
                        &repo.id,
                        &doc,
                        DocumentStateUpdate {
                            stage: JobStage::Complete,
                            hash: Some(&hash),
                            local_path: raw_path.as_deref().and_then(|p| p.to_str()),
                            error: None,
                        },
                    )?;
                    return Ok::<_, anyhow::Error>(());
                }

                let mut markdown = doc
                    .markdown
                    .take()
                    .unwrap_or_else(|| fallback_markdown(&doc));
                self.state.upsert_document(
                    &self.config.host,
                    &repo.id,
                    &doc,
                    DocumentStateUpdate::stage(JobStage::AssetsDownloading),
                )?;
                let markdown_path = self.archive.document_path(team, &snapshot, toc);
                markdown = self
                    .localize_assets(team, repo, &doc.doc_id, &markdown, &markdown_path)
                    .await?;
                markdown =
                    self.localize_document_links(team, &snapshot, &markdown_path, &markdown)?;
                let path = self
                    .archive
                    .save_markdown(&self.config.host, team, &snapshot, toc, &doc, &markdown)
                    .await?;
                let hash = content_hash(markdown.as_bytes());
                self.state.upsert_document(
                    &self.config.host,
                    &repo.id,
                    &doc,
                    DocumentStateUpdate {
                        stage: JobStage::Complete,
                        hash: Some(&hash),
                        local_path: path.to_str(),
                        error: None,
                    },
                )?;
                self.state.record_file(&ArchivedFileRecord {
                    host: &self.config.host,
                    repo_id: &repo.id,
                    doc_id: &doc.doc_id,
                    path: &path,
                    sha256: &hash,
                    size: markdown.len() as u64,
                    kind: "markdown",
                })?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let success = result.is_ok();
            match result {
                Ok(()) => {
                    let outcome = resume_plan.mark_downloaded(ordinal);
                    if outcome.was_failed {
                        repo_stats.failed = repo_stats.failed.saturating_sub(1);
                        summary.document_failed = summary.document_failed.saturating_sub(1);
                    }
                    if !outcome.was_done && !outcome.was_failed {
                        repo_stats.completed += 1;
                        summary.document_completed += 1;
                    }
                    repo_stats.downloaded += 1;
                    summary.document_downloaded += 1;
                }
                Err(error) => {
                    let outcome = resume_plan.mark_failed(ordinal);
                    if !outcome.was_done && !outcome.was_failed {
                        repo_stats.completed += 1;
                        repo_stats.failed += 1;
                        summary.document_completed += 1;
                        summary.document_failed += 1;
                    }
                    let placeholder = crate::models::DocumentPayload {
                        doc_id: toc.id.clone(),
                        slug: toc.slug.clone().unwrap_or_default(),
                        title: toc.title.clone(),
                        updated_at: None,
                        content_updated_at: None,
                        markdown: None,
                        body_html: None,
                        body_lake: None,
                        sheet: None,
                        raw: serde_json::Value::Null,
                        diagram_raw: None,
                    };
                    let error_text = error.to_string();
                    self.state.upsert_document(
                        &self.config.host,
                        &repo.id,
                        &placeholder,
                        DocumentStateUpdate {
                            stage: JobStage::Failed,
                            hash: None,
                            local_path: None,
                            error: Some(&error_text),
                        },
                    )?;
                    tracing::error!(repo=%repo.namespace, doc=%toc.title, %error, "文档下载失败");
                }
            }
            emit_progress(
                emit,
                ProgressContext {
                    summary,
                    team,
                    repo,
                    repository_index,
                    repository_total,
                    repo_stats: &repo_stats,
                },
                &toc.title,
                if success { "完成" } else { "失败" },
                if success { "完成" } else { "失败" },
            );
            processed_since_flush += 1;
            if processed_since_flush >= 50 {
                resume_plan.flush(&self.state, &self.config.host, &repo.id)?;
                processed_since_flush = 0;
            }
        }
        resume_plan.flush(&self.state, &self.config.host, &repo.id)?;
        Ok(())
    }

    async fn localize_assets(
        &self,
        team: &Team,
        repo: &Repository,
        doc_id: &str,
        markdown: &str,
        markdown_path: &std::path::Path,
    ) -> Result<String> {
        let urls = markdown_urls(
            markdown,
            self.config.sync.download_images,
            self.config.sync.download_attachments,
        )?;
        if urls.is_empty() {
            return Ok(markdown.to_string());
        }
        let output_dir = self.archive.asset_dir(team, repo, doc_id);
        tokio::fs::create_dir_all(&output_dir).await?;
        let concurrency = self.config.rate_limit.asset_concurrency.max(1);
        let results = stream::iter(urls.into_iter().map(|url| {
            let output_dir = output_dir.clone();
            async move {
                self.download_asset(&url, &output_dir)
                    .await
                    .map(|asset| (url, asset))
            }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
        let mut localized = markdown.to_string();
        for result in results {
            match result {
                Ok((url, asset)) => {
                    let parent = markdown_path
                        .parent()
                        .ok_or_else(|| anyhow!("文档路径缺少父目录"))?;
                    let relative = pathdiff::diff_paths(&asset.path, parent)
                        .ok_or_else(|| anyhow!("无法计算资源相对路径"))?;
                    localized =
                        localized.replace(&url, &relative.to_string_lossy().replace('\\', "/"));
                    self.state.record_file(&ArchivedFileRecord {
                        host: &self.config.host,
                        repo_id: &repo.id,
                        doc_id,
                        path: &asset.path,
                        sha256: &asset.hash,
                        size: asset.size,
                        kind: "asset",
                    })?;
                }
                Err(error) => tracing::warn!(%error, "资源下载失败，保留远程链接"),
            }
        }
        Ok(localized)
    }

    async fn download_asset(
        &self,
        url: &str,
        output_dir: &std::path::Path,
    ) -> Result<DownloadedAsset> {
        let mut parsed = reqwest::Url::parse(url)?;
        if parsed.path().contains("/attachments/")
            && !parsed.path().contains("/api/v2/attachments/")
        {
            let rewritten = parsed
                .path()
                .replacen("/attachments/", "/api/v2/attachments/", 1);
            parsed.set_path(&rewritten);
        }
        for attempt in 0..3 {
            let _permit = self.asset_limiter.acquire().await?;
            let mut request = self
                .asset_client
                .get(parsed.clone())
                .header(USER_AGENT, "yuque-backup/0.1");
            if parsed.host_str() == self.config.origin()?.host_str() {
                if let Some(cookie) = self.config.cookie_header() {
                    request = request.header(COOKIE, cookie);
                }
                if let Some(token) = self.config.token() {
                    request = request.header("X-Auth-Token", token);
                }
            }
            let response = request.send().await?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let wait = retry_after(&response).unwrap_or(Duration::from_secs(60));
                self.asset_limiter
                    .notify_server_retry_after(wait, Some(parsed.to_string()));
                tokio::time::sleep(wait).await;
                if attempt < 2 {
                    continue;
                }
            }
            let response = response.error_for_status()?;
            if response.url().path().contains("login") {
                return Err(anyhow!("资源需要登录，但认证没有被目标 Host 接受: {url}"));
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = response.bytes().await?;
            let hash = format!("{:x}", Sha256::digest(&bytes));
            let size = bytes.len() as u64;
            let extension = asset_extension(&parsed, &content_type);
            let name = format!("{:x}.{}", Sha256::digest(url.as_bytes()), extension);
            let path = output_dir.join(name);
            if !path.exists() {
                atomic_write(&path, &bytes).await?;
            }
            return Ok(DownloadedAsset { path, hash, size });
        }
        Err(anyhow!("资源请求持续受限: {url}"))
    }

    async fn record_existing_file(
        &self,
        repo: &Repository,
        doc_id: &str,
        path: &std::path::Path,
        kind: &str,
    ) -> Result<()> {
        let bytes = tokio::fs::read(path).await?;
        let hash = content_hash(&bytes);
        self.state.record_file(&ArchivedFileRecord {
            host: &self.config.host,
            repo_id: &repo.id,
            doc_id,
            path,
            sha256: &hash,
            size: bytes.len() as u64,
            kind,
        })
    }

    async fn diagram_backfill_ordinals(
        &self,
        team: &Team,
        repo: &Repository,
        docs: &[&crate::models::TocItem],
        local_states: &HashMap<String, DocumentResumeState>,
    ) -> Result<Vec<usize>> {
        let mut output = Vec::new();
        let repo_dir = self.archive.repository_dir(team, repo);
        for (ordinal, toc) in docs.iter().enumerate() {
            let Some(local) = local_states.get(&toc.id) else {
                continue;
            };
            if local.stage != JobStage::Complete {
                continue;
            }
            let diagram_report = repo_dir
                .join("diagrams")
                .join(safe_component(&toc.id))
                .join("diagram-report.json");
            if diagram_report.exists() {
                continue;
            }
            if self
                .has_local_diagram_hint(&repo_dir, toc)
                .await
                .unwrap_or(false)
            {
                output.push(ordinal);
            }
        }
        Ok(output)
    }

    async fn has_local_diagram_hint(
        &self,
        repo_dir: &std::path::Path,
        toc: &crate::models::TocItem,
    ) -> Result<bool> {
        if contains_diagram_hint(&toc.title)
            || toc
                .slug
                .as_deref()
                .map(contains_diagram_hint)
                .unwrap_or(false)
            || serde_json::to_string(&toc.raw)
                .map(|text| contains_diagram_hint(&text))
                .unwrap_or(false)
        {
            return Ok(true);
        }

        let raw_path = repo_dir
            .join("raw")
            .join("docs")
            .join(format!("{}.json", safe_component(&toc.id)));
        let Ok(text) = tokio::fs::read_to_string(raw_path).await else {
            return Ok(false);
        };
        Ok(contains_diagram_hint(&text))
    }

    fn localize_document_links(
        &self,
        team: &Team,
        snapshot: &crate::models::RepositorySnapshot,
        current_path: &std::path::Path,
        markdown: &str,
    ) -> Result<String> {
        let regex = Regex::new(r#"\[[^\]]*\]\((https?://[^\s)>]+)"#)?;
        let configured_host = self.config.origin()?.host_str().map(str::to_owned);
        let mut localized = markdown.to_string();
        for capture in regex.captures_iter(markdown) {
            let Some(raw_url) = capture.get(1).map(|v| v.as_str()) else {
                continue;
            };
            let Ok(url) = reqwest::Url::parse(raw_url) else {
                continue;
            };
            if url.host_str() != configured_host.as_deref() {
                continue;
            }
            let segments: Vec<_> = url
                .path_segments()
                .map(|segments| segments.collect())
                .unwrap_or_default();
            if segments.len() < 3
                || segments[0] != snapshot.repository.owner_login
                || segments[1] != snapshot.repository.slug
            {
                continue;
            }
            let Some(target) = snapshot
                .toc
                .iter()
                .find(|item| item.slug.as_deref() == Some(segments[2]))
            else {
                continue;
            };
            let target_path = self.archive.document_path(team, snapshot, target);
            let Some(parent) = current_path.parent() else {
                continue;
            };
            let Some(relative) = pathdiff::diff_paths(target_path, parent) else {
                continue;
            };
            let mut replacement = relative.to_string_lossy().replace('\\', "/");
            if let Some(fragment) = url.fragment() {
                replacement.push('#');
                replacement.push_str(fragment);
            }
            localized = localized.replace(raw_url, &replacement);
        }
        Ok(localized)
    }
}

fn contains_diagram_hint(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("lakemind")
        || lower.contains("mindmap")
        || lower.contains("mind-map")
        || lower.contains("lakeboard")
        || lower.contains("_lake_card")
        || lower.contains("name=\\\"board")
        || lower.contains("name=\"board")
        || lower.contains("data-lake-card=\\\"board")
        || text.contains("思维导图")
        || text.contains("脑图")
}

struct ProgressContext<'a> {
    summary: &'a SyncSummary,
    team: &'a Team,
    repo: &'a Repository,
    repository_index: usize,
    repository_total: usize,
    repo_stats: &'a RepositorySyncStats,
}

fn emit_progress(
    emit: &mut impl FnMut(SyncEvent),
    context: ProgressContext<'_>,
    document: &str,
    stage: &str,
    message: &str,
) {
    emit(SyncEvent::Progress(SyncProgress {
        team: context.team.name.clone(),
        repository: context.repo.name.clone(),
        document: document.to_string(),
        stage: stage.to_string(),
        repository_index: context.repository_index,
        repository_total: context.repository_total,
        repository_completed: context.summary.repository_completed,
        completed: context.repo_stats.completed,
        total: context.repo_stats.total,
        failed: context.repo_stats.failed,
        skipped: context.repo_stats.skipped,
        downloaded: context.repo_stats.downloaded,
        overall_completed: context.summary.document_completed,
        overall_total: context.summary.document_total,
        overall_failed: context.summary.document_failed,
        overall_skipped: context.summary.document_skipped,
        overall_downloaded: context.summary.document_downloaded,
        message: message.to_string(),
    }));
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn markdown_urls(markdown: &str, images: bool, attachments: bool) -> Result<Vec<String>> {
    let regex = Regex::new(r#"(!?)\[[^\]]*\]\((https?://[^\s)>]+)"#)?;
    let mut seen = HashSet::new();
    Ok(regex
        .captures_iter(markdown)
        .filter_map(|c| {
            let is_image = c.get(1).is_some_and(|v| v.as_str() == "!");
            let url = c.get(2)?.as_str();
            ((images && is_image) || (attachments && url.contains("/attachments/")))
                .then(|| url.to_string())
        })
        .filter(|url| seen.insert(url.clone()))
        .collect())
}

fn asset_extension(url: &reqwest::Url, content_type: &str) -> String {
    let from_url = url
        .path_segments()
        .and_then(|mut p| p.next_back())
        .and_then(|name| name.rsplit_once('.').map(|(_, ext)| ext))
        .filter(|ext| ext.len() <= 8);
    from_url
        .unwrap_or_else(|| match content_type.split(';').next().unwrap_or("") {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/svg+xml" => "svg",
            "application/pdf" => "pdf",
            _ => "bin",
        })
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn fallback_markdown(doc: &crate::models::DocumentPayload) -> String {
    if doc.sheet.is_some() {
        format!(
            "# {}\n\n> 此文档是语雀表格。完整原始数据及 CSV 位于 `tables/{}`。\n",
            doc.title, doc.doc_id
        )
    } else if doc.body_html.is_some() || doc.body_lake.is_some() {
        format!(
            "# {}\n\n> 此文档无法无损转换为 Markdown，原始 HTML/Lake 数据已保存在 `raw/docs`。\n",
            doc.title
        )
    } else {
        format!(
            "# {}\n\n> 语雀接口未返回可渲染正文，原始响应已归档。\n",
            doc.title
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_requested_resource_types() {
        let markdown = "![图](https://cdn.example/a.png) [附件](https://x.yuque.com/attachments/a.pdf) [网页](https://example.com)";
        let all = markdown_urls(markdown, true, true).unwrap();
        assert_eq!(all.len(), 2);
        let only_images = markdown_urls(markdown, true, false).unwrap();
        assert_eq!(only_images, vec!["https://cdn.example/a.png"]);
    }
}
