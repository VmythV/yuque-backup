use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::cli::Cli;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub host: String,
    pub output_dir: PathBuf,
    pub auth: AuthConfig,
    pub rate_limit: RateLimitConfig,
    pub sync: SyncConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub cookie_key: String,
    pub cookie_env: String,
    pub token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub api_requests_per_hour: u32,
    pub api_concurrency: usize,
    pub minimum_interval_ms: u64,
    pub asset_concurrency: usize,
    pub asset_minimum_interval_ms: u64,
    pub reserve_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub download_images: bool,
    pub download_attachments: bool,
    pub keep_raw: bool,
    pub render_markdown: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: "https://yuque.com".into(),
            output_dir: PathBuf::from("./backup"),
            auth: AuthConfig::default(),
            rate_limit: RateLimitConfig::default(),
            sync: SyncConfig::default(),
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            cookie_key: "_yuque_session".into(),
            cookie_env: "YUQUE_COOKIE".into(),
            token_env: "YUQUE_TOKEN".into(),
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            api_requests_per_hour: 600,
            api_concurrency: 1,
            minimum_interval_ms: 1_500,
            asset_concurrency: 3,
            asset_minimum_interval_ms: 200,
            reserve_ratio: 0.15,
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            download_images: true,
            download_attachments: true,
            keep_raw: true,
            render_markdown: true,
        }
    }
}

impl AppConfig {
    pub fn load(cli: &Cli) -> Result<Self> {
        let path = cli
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from("yuque-backup.toml"));
        let mut config = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("配置文件格式错误: {}", path.display()))?
        } else {
            Self::default()
        };

        let env_host = env::var("YUQUE_HOST").ok();
        if let Some(host) = cli.host.as_ref().or(env_host.as_ref()) {
            config.host = host.clone();
        }
        if let Some(output) = &cli.output {
            config.output_dir = output.clone();
        }
        config.host = normalize_host(&config.host)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let parsed = Url::parse(&self.host).context("Host 必须是完整 URL")?;
        if parsed.scheme() != "https"
            && parsed.host_str() != Some("localhost")
            && parsed.host_str() != Some("127.0.0.1")
        {
            bail!("Host 必须使用 HTTPS（本地测试地址除外）");
        }
        if self.rate_limit.api_requests_per_hour == 0 {
            bail!("api_requests_per_hour 必须大于 0");
        }
        if !(0.0..0.95).contains(&self.rate_limit.reserve_ratio) {
            bail!("reserve_ratio 必须在 0 到 0.95 之间");
        }
        Ok(())
    }

    pub fn origin(&self) -> Result<Url> {
        Url::parse(&format!("{}/", self.host)).context("无效 Host")
    }

    pub fn url(&self, path: &str) -> Result<Url> {
        self.origin()?
            .join(path.trim_start_matches('/'))
            .context("无法构造语雀 URL")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.output_dir.join(".state")
    }

    pub fn database_path(&self) -> PathBuf {
        self.state_dir().join("state.sqlite3")
    }

    pub fn cookie_header(&self) -> Option<String> {
        env::var(&self.auth.cookie_env)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|value| {
                if value.contains('=') {
                    value
                } else {
                    format!("{}={value}", self.auth.cookie_key)
                }
            })
    }

    pub fn token(&self) -> Option<String> {
        env::var(&self.auth.token_env)
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    pub fn write_template(path: &Path, host: &str, force: bool) -> Result<()> {
        if path.exists() && !force {
            bail!("配置文件已存在: {}，使用 --force 覆盖", path.display());
        }
        let config = Self {
            host: normalize_host(host)?,
            ..Self::default()
        };
        fs::write(path, toml::to_string_pretty(&config)?)
            .with_context(|| format!("写入配置失败: {}", path.display()))
    }
}

pub fn normalize_host(input: &str) -> Result<String> {
    let value = input.trim().trim_end_matches('/');
    let parsed = Url::parse(value).with_context(|| format!("无效 Host: {input}"))?;
    if parsed.host_str().is_none() || parsed.cannot_be_a_base() {
        bail!("Host 必须包含协议和域名");
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        bail!("Host 只能包含 origin，不能带路径: {input}");
    }
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap(),
        port
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_host() {
        assert_eq!(
            normalize_host("https://yuque.com/").unwrap(),
            "https://yuque.com"
        );
        assert!(normalize_host("https://yuque.com/path").is_err());
    }
}
