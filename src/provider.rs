use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{
    Client, Response, StatusCode,
    header::{COOKIE, RETRY_AFTER, USER_AGENT},
};
use serde_json::Value;

use crate::{
    config::AppConfig,
    models::{
        DocumentPayload, RemoteDocumentMeta, Repository, RepositorySnapshot, Team, TeamKind,
        TocItem,
    },
    rate_limit::{PersistentRateLimiter, RateLimitCallback},
    state::StateStore,
};

#[async_trait]
pub trait YuqueProvider: Send + Sync {
    async fn validate_session(&self) -> Result<()>;
    async fn discover(&self) -> Result<Vec<Team>>;
    async fn repository_snapshot(&self, repo: &Repository) -> Result<RepositorySnapshot>;
    async fn document_index(
        &self,
        repo: &Repository,
    ) -> Result<HashMap<String, RemoteDocumentMeta>>;
    async fn document(&self, repo: &Repository, toc: &TocItem) -> Result<DocumentPayload>;
}

#[derive(Clone)]
pub struct CookieProvider {
    config: AppConfig,
    client: Client,
    limiter: PersistentRateLimiter,
}

impl CookieProvider {
    pub fn new(config: AppConfig, state: StateStore) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        let limiter = PersistentRateLimiter::api(config.host.clone(), &config.rate_limit, state);
        Ok(Self {
            config,
            client,
            limiter,
        })
    }

    pub fn set_rate_limit_callback(&self, callback: Option<RateLimitCallback>) {
        self.limiter.set_callback(callback);
    }

    async fn get(&self, path_or_url: &str) -> Result<Response> {
        let url = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            reqwest::Url::parse(path_or_url)?
        } else {
            self.config.url(path_or_url)?
        };

        let mut last_error = None;
        for attempt in 0..5_u32 {
            let _permit = self.limiter.acquire().await?;
            let mut request = self.client.get(url.clone()).header(
                USER_AGENT,
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/126 Safari/537.36 yuque-backup/0.1",
            );
            if let Some(cookie) = self.config.cookie_header() {
                request = request.header(COOKIE, cookie);
            }
            if let Some(token) = self.config.token() {
                request = request.header("X-Auth-Token", token);
            }

            match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
                    let wait = retry_after(&response).unwrap_or(Duration::from_secs(60));
                    self.limiter
                        .notify_server_retry_after(wait, Some(url.to_string()));
                    tracing::warn!(url=%url, ?wait, "语雀返回 429，等待额度恢复");
                    tokio::time::sleep(wait).await;
                    last_error = Some(anyhow!("请求受限: {url}"));
                }
                Ok(response) if response.status().is_server_error() => {
                    last_error = Some(anyhow!("服务器错误 {}: {url}", response.status()));
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Ok(response) => {
                    let status = response.status();
                    let final_url = response.url().clone();
                    let body = response.text().await.unwrap_or_default();
                    bail!("请求失败 {status}: {final_url}: {}", truncate(&body, 300));
                }
                Err(error) => {
                    last_error = Some(error.into());
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("请求失败: {url}")))
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let response = self.get(path).await?;
        let final_url = response.url().clone();
        response
            .json()
            .await
            .with_context(|| format!("响应不是 JSON: {final_url}"))
    }

    async fn optional_json(&self, path: &str) -> Option<Value> {
        match self.get_json(path).await {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%path, %error, "可选发现接口不可用");
                None
            }
        }
    }
}

#[async_trait]
impl YuqueProvider for CookieProvider {
    async fn validate_session(&self) -> Result<()> {
        if self.config.cookie_header().is_none() && self.config.token().is_none() {
            bail!(
                "未找到认证信息。请设置 {} 或 {}",
                self.config.auth.cookie_env,
                self.config.auth.token_env
            );
        }
        let response = self.get("/api/mine/books?limit=1&offset=0").await?;
        if response.url().path().contains("login") {
            bail!("登录态已失效，请更新 {}", self.config.auth.cookie_env);
        }
        Ok(())
    }

    async fn discover(&self) -> Result<Vec<Team>> {
        let sources = [
            ("/api/mine/book_stacks", TeamKind::Personal),
            ("/api/mine/user_books?user_type=Group", TeamKind::Group),
            ("/api/mine/raw_collab_books", TeamKind::Collaboration),
            ("/api/mine/books?limit=100&offset=0", TeamKind::Personal),
        ];
        let mut teams: BTreeMap<(String, String), Team> = BTreeMap::new();

        for (path, kind) in sources {
            let Some(response) = self.optional_json(path).await else {
                continue;
            };
            let mut books = Vec::new();
            collect_books(response.get("data").unwrap_or(&response), &mut books);
            for value in books {
                let Some(repo) = repository_from_value(value) else {
                    continue;
                };
                let owner = value
                    .get("user")
                    .or_else(|| value.get("owner"))
                    .unwrap_or(&Value::Null);
                let owner_id =
                    value_string(owner, "id").unwrap_or_else(|| repo.owner_login.clone());
                let owner_name =
                    value_string(owner, "name").unwrap_or_else(|| repo.owner_login.clone());
                let key = (kind.as_str().to_string(), owner_id.clone());
                let team = teams.entry(key).or_insert_with(|| Team {
                    id: owner_id,
                    login: repo.owner_login.clone(),
                    name: owner_name,
                    kind: kind.clone(),
                    repositories: Vec::new(),
                });
                if !team.repositories.iter().any(|item| item.id == repo.id) {
                    team.repositories.push(repo);
                }
            }
        }

        let mut result: Vec<_> = teams
            .into_values()
            .filter(|team| !team.repositories.is_empty())
            .collect();
        dedupe_repositories_across_teams(&mut result);
        result.retain(|team| !team.repositories.is_empty());
        for team in &mut result {
            team.repositories.sort_by(|a, b| a.name.cmp(&b.name));
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        if result.is_empty() {
            bail!("没有发现可访问的知识库，请检查 Host 和 Cookie 权限");
        }
        Ok(result)
    }

    async fn repository_snapshot(&self, repo: &Repository) -> Result<RepositorySnapshot> {
        let response = self
            .get(&format!("/{}/{}", repo.owner_login, repo.slug))
            .await?;
        let html = response.text().await?;
        let app_data = parse_app_data(&html)?;
        let toc_value = app_data
            .pointer("/book/toc")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let toc = toc_value.iter().filter_map(toc_from_value).collect();
        Ok(RepositorySnapshot {
            repository: repo.clone(),
            toc,
            raw_app_data: app_data,
        })
    }

    async fn document_index(
        &self,
        repo: &Repository,
    ) -> Result<HashMap<String, RemoteDocumentMeta>> {
        let mut output = HashMap::new();
        let mut offset = 0_u32;
        loop {
            let raw = self
                .get_json(&format!(
                    "/api/docs?book_id={}&limit=100&offset={offset}",
                    repo.id
                ))
                .await?;
            let data = raw
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in &data {
                let Some(slug) = value_string(item, "slug").or_else(|| value_string(item, "url"))
                else {
                    continue;
                };
                output.insert(
                    slug.clone(),
                    RemoteDocumentMeta {
                        id: value_string(item, "id").unwrap_or_default(),
                        slug,
                        updated_at: value_string(item, "updated_at"),
                        content_updated_at: value_string(item, "content_updated_at"),
                    },
                );
            }
            if data.len() < 100 {
                break;
            }
            offset += 100;
        }
        Ok(output)
    }

    async fn document(&self, repo: &Repository, toc: &TocItem) -> Result<DocumentPayload> {
        let slug = toc
            .slug
            .as_deref()
            .ok_or_else(|| anyhow!("文档缺少 slug: {}", toc.title))?;
        let path = format!(
            "/api/docs/{slug}?book_id={}&merge_dynamic_data=false&mode=markdown",
            repo.id
        );
        let raw = self.get_json(&path).await?;
        let data = raw.get("data").unwrap_or(&raw);
        let content_json = data
            .get("content")
            .and_then(Value::as_str)
            .and_then(|v| serde_json::from_str::<Value>(v).ok());
        let sheet = data
            .get("sheet")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                content_json
                    .as_ref()
                    .and_then(|v| v.get("sheet"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let diagram_raw = if should_fetch_diagram_payload(data, &raw) {
            let path = format!(
                "/api/docs/{slug}?book_id={}&merge_dynamic_data=false",
                repo.id
            );
            match self.get_json(&path).await {
                Ok(value) => Some(value),
                Err(error) => {
                    tracing::warn!(slug=%slug, %error, "思维导图/画板默认 API 探测失败，继续保存基础文档");
                    None
                }
            }
        } else {
            None
        };
        Ok(DocumentPayload {
            doc_id: value_string(data, "id").unwrap_or_else(|| toc.id.clone()),
            slug: slug.to_string(),
            title: value_string(data, "title").unwrap_or_else(|| toc.title.clone()),
            updated_at: value_string(data, "updated_at"),
            content_updated_at: value_string(data, "content_updated_at"),
            markdown: data
                .get("sourcecode")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| data.get("body").and_then(Value::as_str).map(str::to_owned)),
            body_html: data
                .get("body_html")
                .and_then(Value::as_str)
                .map(str::to_owned),
            body_lake: data
                .get("body_lake")
                .or_else(|| data.get("body_draft_lake"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            sheet,
            raw,
            diagram_raw,
        })
    }
}

fn dedupe_repositories_across_teams(teams: &mut [Team]) {
    let mut team_order = (0..teams.len()).collect::<Vec<_>>();
    team_order.sort_by_key(|index| team_kind_priority(&teams[*index].kind));

    let mut seen = HashSet::new();
    for index in team_order {
        teams[index]
            .repositories
            .retain(|repo| seen.insert(repo.id.clone()));
    }
}

fn team_kind_priority(kind: &TeamKind) -> u8 {
    match kind {
        TeamKind::Group => 0,
        TeamKind::Collaboration => 1,
        TeamKind::Personal => 2,
    }
}

fn should_fetch_diagram_payload(data: &Value, raw: &Value) -> bool {
    let doc_type = value_string(data, "type").unwrap_or_default();
    let format = value_string(data, "format").unwrap_or_default();
    if doc_type.eq_ignore_ascii_case("board") || format.eq_ignore_ascii_case("lakeboard") {
        return true;
    }

    for value in [
        data.get("sourcecode").and_then(Value::as_str),
        data.get("body").and_then(Value::as_str),
        data.get("title").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    {
        if contains_diagram_hint(value) {
            return true;
        }
    }

    serde_json::to_string(raw)
        .map(|text| contains_diagram_hint(&text))
        .unwrap_or(false)
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

fn collect_books<'a>(value: &'a Value, output: &mut Vec<&'a Value>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect_books(item, output)),
        Value::Object(map) => {
            if map.contains_key("id") && map.contains_key("slug") && map.contains_key("name") {
                let kind = map.get("type").and_then(Value::as_str).unwrap_or("Book");
                if kind == "Book" {
                    output.push(value);
                }
                return;
            }
            for key in ["books", "items", "data"] {
                if let Some(child) = map.get(key) {
                    collect_books(child, output);
                }
            }
        }
        _ => {}
    }
}

fn repository_from_value(value: &Value) -> Option<Repository> {
    let id = value_string(value, "id")?;
    let slug = value_string(value, "slug")?;
    let name = value_string(value, "name")?;
    let owner = value.get("user").or_else(|| value.get("owner"))?;
    let owner_login = value_string(owner, "login")?;
    Some(Repository {
        namespace: value_string(value, "namespace")
            .unwrap_or_else(|| format!("{owner_login}/{slug}")),
        id,
        slug,
        name,
        owner_login,
        description: value_string(value, "description"),
        items_count: value.get("items_count").and_then(Value::as_u64),
        selected: false,
    })
}

fn toc_from_value(value: &Value) -> Option<TocItem> {
    let item_type = value_string(value, "type").unwrap_or_else(|| "DOC".into());
    let uuid = value_string(value, "uuid")
        .or_else(|| value_string(value, "id"))
        .unwrap_or_default();
    let id = value_string(value, "doc_id")
        .or_else(|| value_string(value, "id"))
        .unwrap_or_else(|| uuid.clone());
    Some(TocItem {
        id,
        uuid,
        parent_uuid: value_string(value, "parent_uuid").filter(|v| !v.is_empty()),
        title: value_string(value, "title")?,
        slug: value_string(value, "url")
            .or_else(|| value_string(value, "slug"))
            .filter(|v| !v.is_empty()),
        visible: value.get("visible").and_then(Value::as_i64).unwrap_or(1) == 1,
        item_type,
        raw: value.clone(),
    })
}

fn value_string(value: &Value, key: &str) -> Option<String> {
    let value = value.get(key)?;
    match value {
        Value::String(v) => Some(v.clone()),
        Value::Number(v) => Some(v.to_string()),
        _ => None,
    }
}

pub fn parse_app_data(html: &str) -> Result<Value> {
    let regex = Regex::new(r#"decodeURIComponent\("((?:\\.|[^"])*)"\)"#)?;
    let captures = regex
        .captures(html)
        .ok_or_else(|| anyhow!("页面中未找到 appData"))?;
    let encoded_js = captures.get(1).unwrap().as_str();
    let js_string: String = serde_json::from_str(&format!("\"{encoded_js}\""))
        .context("appData JavaScript 字符串解码失败")?;
    let decoded = percent_decode_str(&js_string)
        .decode_utf8()
        .context("appData URL 解码失败")?;
    serde_json::from_str(&decoded).context("appData JSON 解析失败")
}

fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(2_u64.saturating_pow(attempt.min(5)))
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_embedded_app_data() {
        let html = r#"<script>window.appData=JSON.parse(decodeURIComponent("%7B%22book%22%3A%7B%22toc%22%3A%5B%5D%7D%7D"));</script>"#;
        let parsed = parse_app_data(html).unwrap();
        assert!(parsed.pointer("/book/toc").unwrap().is_array());
    }

    #[test]
    fn dedupes_repositories_across_team_kinds() {
        let repo = Repository {
            id: "repo-1".into(),
            slug: "repo".into(),
            name: "Repo".into(),
            namespace: "team/repo".into(),
            owner_login: "team".into(),
            description: None,
            items_count: None,
            selected: false,
        };
        let mut teams = vec![
            Team {
                id: "personal".into(),
                login: "team".into(),
                name: "Team".into(),
                kind: TeamKind::Personal,
                repositories: vec![repo.clone()],
            },
            Team {
                id: "group".into(),
                login: "team".into(),
                name: "Team".into(),
                kind: TeamKind::Group,
                repositories: vec![repo],
            },
        ];

        dedupe_repositories_across_teams(&mut teams);

        assert!(teams[0].repositories.is_empty());
        assert_eq!(teams[1].repositories.len(), 1);
    }
}
