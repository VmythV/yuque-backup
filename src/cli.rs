use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::{
    config::AppConfig,
    downloader::Downloader,
    models::{SyncEvent, Team},
    provider::{CookieProvider, YuqueProvider},
    rate_limit::RateLimitCallback,
    state::{StateStore, now_epoch_ms},
    tui::{confirm_resume, select_repositories, show_sync_progress},
};

#[derive(Debug, Parser)]
#[command(
    name = "yuque-backup",
    version,
    about = "可断点续传、持久化限流的语雀本地备份工具"
)]
pub struct Cli {
    /// 配置文件路径
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// 语雀空间 Host，例如 https://yuque.com
    #[arg(long, global = true, env = "YUQUE_HOST")]
    pub host: Option<String>,
    /// 备份输出目录
    #[arg(long, global = true)]
    pub output: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 生成配置文件
    Init {
        #[arg(long, default_value = "https://yuque.com")]
        host: String,
        #[arg(long)]
        force: bool,
    },
    /// 打开团队/知识库选择界面并开始同步
    Tui {
        /// 只保存选择，不开始下载
        #[arg(long)]
        select_only: bool,
    },
    /// 输出当前账号可访问的团队和知识库
    Discover {
        #[arg(long)]
        json: bool,
    },
    /// 同步已选知识库
    Sync {
        /// 忽略已保存选择，同步发现的所有知识库
        #[arg(long)]
        all: bool,
    },
    /// 查看断点和失败状态
    Status,
    /// 检查归档清单和未完成临时文件
    Verify,
}

pub async fn run(cli: Cli, config: AppConfig) -> Result<()> {
    match cli.command.unwrap_or(Command::Tui { select_only: false }) {
        Command::Init { host, force } => {
            let path = cli
                .config
                .as_deref()
                .unwrap_or(Path::new("yuque-backup.toml"));
            AppConfig::write_template(path, &host, force)?;
            println!("配置已生成: {}", path.display());
            println!(
                "请设置环境变量 {} 后运行 yuque-backup tui",
                config.auth.cookie_env
            );
            Ok(())
        }
        Command::Status => show_status(&config),
        Command::Verify => verify(&config),
        command => {
            let state = StateStore::open(config.database_path())?;
            warn_if_api_rate_limited(&config, &state)?;
            let provider = Arc::new(CookieProvider::new(config.clone(), state.clone())?);
            provider.validate_session().await?;
            match command {
                Command::Discover { json } => {
                    let teams = provider.discover().await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&teams)?);
                    } else {
                        print_discovery(&teams);
                    }
                    Ok(())
                }
                Command::Tui { select_only } => {
                    let mut teams = provider.discover().await?;
                    if !select_repositories(&config.host, &mut teams, &state)? {
                        return Ok(());
                    }
                    persist_selections(&config.host, &teams, &state)?;
                    if !has_selected_repository(&teams) {
                        bail!("没有选择任何知识库，请至少选择一个知识库后再开始同步");
                    }
                    if select_only {
                        return Ok(());
                    }
                    if !confirm_resume(&config.host, &teams, &state)? {
                        return Ok(());
                    }
                    sync_selected(config, state, provider, teams, true).await
                }
                Command::Sync { all } => {
                    let mut teams = provider.discover().await?;
                    if all {
                        teams
                            .iter_mut()
                            .flat_map(|t| &mut t.repositories)
                            .for_each(|r| r.selected = true);
                    } else {
                        let ids: std::collections::HashSet<_> =
                            state.selected_repo_ids(&config.host)?.into_iter().collect();
                        teams
                            .iter_mut()
                            .flat_map(|t| &mut t.repositories)
                            .for_each(|r| r.selected = ids.contains(&r.id));
                    }
                    if !teams
                        .iter()
                        .flat_map(|t| &t.repositories)
                        .any(|r| r.selected)
                    {
                        bail!("没有已选知识库，请先运行 yuque-backup tui");
                    }
                    print_resume_notice(&config.host, &teams, &state)?;
                    sync_selected(config, state, provider, teams, false).await
                }
                _ => unreachable!(),
            }
        }
    }
}

fn warn_if_api_rate_limited(config: &AppConfig, state: &StateStore) -> Result<()> {
    let now = now_epoch_ms();
    let entries = state.request_window(&config.host, "api", now - 3_600_000)?;
    let usable = ((config.rate_limit.api_requests_per_hour as f64)
        * (1.0 - config.rate_limit.reserve_ratio))
        .floor()
        .max(1.0) as usize;

    if entries.len() >= usable {
        let wait_ms = entries
            .first()
            .map(|first| first + 3_600_000 - now + 250)
            .unwrap_or(250)
            .max(250);
        eprintln!(
            "API 限流窗口已满：过去一小时已用 {}/{} 次；预计还需 {} 释放额度。程序会继续等待，不是卡死。",
            entries.len(),
            usable,
            format_wait_duration(wait_ms)
        );
    }

    Ok(())
}

fn format_wait_duration(ms: i64) -> String {
    let total_seconds = ((ms + 999) / 1000).max(1);
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes == 0 {
        format!("{seconds} 秒")
    } else if seconds == 0 {
        format!("{minutes} 分钟")
    } else {
        format!("{minutes} 分 {seconds} 秒")
    }
}

async fn sync_selected(
    config: AppConfig,
    state: StateStore,
    provider: Arc<CookieProvider>,
    teams: Vec<Team>,
    use_tui: bool,
) -> Result<()> {
    if use_tui {
        let (sender, receiver) = std::sync::mpsc::channel();
        let rate_sender = sender.clone();
        let rate_callback: RateLimitCallback = Arc::new(move |event| {
            let _ = rate_sender.send(SyncEvent::RateLimit(event));
        });
        provider.set_rate_limit_callback(Some(rate_callback.clone()));
        let downloader = Downloader::new(config, provider, state, Some(rate_callback))?;
        let task = tokio::spawn(async move {
            let result = downloader
                .sync(&teams, |event| {
                    let _ = sender.send(event);
                })
                .await;
            match &result {
                Ok(summary) => {
                    let _ = sender.send(SyncEvent::Finished {
                        summary: summary.clone(),
                    });
                }
                Err(error) => {
                    let _ = sender.send(SyncEvent::Failed {
                        message: error.to_string(),
                    });
                }
            }
            result
        });
        let completed = tokio::task::block_in_place(|| show_sync_progress(receiver))?;
        if !completed {
            task.abort();
            let _ = task.await;
            println!("本次同步已取消；已完成状态已保存，可再次运行继续。");
            return Ok(());
        }
        task.await??;
        Ok(())
    } else {
        provider.set_rate_limit_callback(None);
        let downloader = Downloader::new(config, provider, state, None)?;
        let summary = downloader
            .sync(&teams, |event| match event {
                SyncEvent::Progress(progress) => {
                    println!(
                        "[知识库 {}/{}，文档 {}/{}] {} / {} / {} — {}（跳过 {}，下载 {}，失败 {}）",
                        progress.repository_index,
                        progress.repository_total,
                        progress.completed,
                        progress.total,
                        progress.team,
                        progress.repository,
                        progress.document,
                        progress.message,
                        progress.skipped,
                        progress.downloaded,
                        progress.failed
                    );
                }
                SyncEvent::RateLimit(limit) => {
                    println!(
                        "{} 限流：{}，等待 {} 秒",
                        limit.bucket,
                        limit.reason.label(),
                        limit.wait_seconds
                    );
                }
                SyncEvent::Warning { message } => println!("警告：{message}"),
                SyncEvent::Finished { .. } | SyncEvent::Failed { .. } => {}
            })
            .await?;
        println!(
            "同步完成：知识库 {}/{}，文档 {}/{}，跳过 {}，下载 {}，失败 {}",
            summary.repository_completed,
            summary.repository_total,
            summary.document_completed,
            summary.document_total,
            summary.document_skipped,
            summary.document_downloaded,
            summary.document_failed
        );
        Ok(())
    }
}

fn has_selected_repository(teams: &[Team]) -> bool {
    teams
        .iter()
        .flat_map(|team| &team.repositories)
        .any(|repo| repo.selected)
}

fn persist_selections(host: &str, teams: &[Team], state: &StateStore) -> Result<()> {
    for repo in teams.iter().flat_map(|t| &t.repositories) {
        state.set_selection(host, repo, repo.selected)?;
    }
    Ok(())
}

fn print_resume_notice(host: &str, teams: &[Team], state: &StateStore) -> Result<()> {
    let progress = state.repository_progress_by_host(host)?;
    let mut repo_count = 0_u64;
    let mut total = 0_u64;
    let mut completed = 0_u64;
    let mut failed = 0_u64;
    let mut in_progress = 0_u64;

    for repo in teams
        .iter()
        .flat_map(|team| &team.repositories)
        .filter(|repo| repo.selected)
    {
        let Some(summary) = progress.get(&repo.id).copied() else {
            continue;
        };
        if !summary.needs_resume() {
            continue;
        }
        repo_count += 1;
        total += summary.total_documents;
        completed += summary.completed_documents;
        failed += summary.failed_documents;
        in_progress += summary.in_progress_documents;
    }

    if repo_count > 0 {
        let pending = total.saturating_sub(completed);
        println!(
            "检测到上次同步未完成：{repo_count} 个知识库待继续，已完成 {completed}/{total}，待处理 {pending}，失败 {failed}，中断 {in_progress}。将自动从断点继续。"
        );
    }

    Ok(())
}

fn print_discovery(teams: &[Team]) {
    for team in teams {
        println!("{} [{}]", team.name, team.kind.as_str());
        for repo in &team.repositories {
            println!("  - {} ({})", repo.name, repo.namespace);
        }
    }
}

fn show_status(config: &AppConfig) -> Result<()> {
    let state = StateStore::open(config.database_path())?;
    let status = state.status(&config.host)?;
    println!("Host: {}", config.host);
    println!("已选知识库: {}", status.selected_repositories);
    println!(
        "文档: {}，完成: {}，失败: {}",
        status.total_documents, status.completed_documents, status.failed_documents
    );
    Ok(())
}

fn verify(config: &AppConfig) -> Result<()> {
    let manifest = config.output_dir.join("manifest.json");
    if !manifest.exists() {
        bail!("未找到归档清单: {}", manifest.display())
    }
    let _: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest)?).context("manifest.json 损坏")?;
    let mut parts = Vec::new();
    collect_part_files(&config.output_dir, &mut parts)?;
    let state = StateStore::open(config.database_path())?;
    let integrity_issues = state.verify_files(&config.host)?;
    if parts.is_empty() && integrity_issues.is_empty() {
        println!("归档清单有效，文件 SHA-256 校验通过，未发现未完成的 .part 文件");
    } else {
        if !parts.is_empty() {
            println!("发现 {} 个未完成临时文件:", parts.len());
            parts.iter().for_each(|p| println!("  {}", p.display()));
        }
        if !integrity_issues.is_empty() {
            println!("发现 {} 个文件完整性问题:", integrity_issues.len());
            integrity_issues
                .iter()
                .for_each(|issue| println!("  {}: {}", issue.path.display(), issue.reason));
        }
        bail!("归档校验未通过");
    }
    Ok(())
}

fn collect_part_files(dir: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_part_files(&path, output)?;
        } else if path.extension().and_then(|v| v.to_str()) == Some("part") {
            output.push(path);
        }
    }
    Ok(())
}
