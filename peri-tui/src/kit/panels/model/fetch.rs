//! 端点模型列表拉取 + env provider 落地（pi 式列表的能力补齐）。
//!
//! ## 为什么需要
//!
//! pi 的 `/model` 列出的是「所有 provider 的可用模型」。peri 这边模型列表有两个来源：
//! 1. 宿主配置 `~/.peri/settings.json` 的 `providers[].models`（四档映射 + profile 手填）；
//! 2. 端点本身：`GET {baseUrl}/models`（OpenAI 兼容 / Anthropic 皆是 `{"data":[{"id":…}]}`）。
//!
//! 只靠 1 时，用环境变量起 peri 的场景（`OPENAI_BASE_URL` / `OPENAI_MODEL` / `OPENAI_API_KEY`，
//! 见 `LlmProvider::from_env`）配置里没有 provider —— 列表会是空的。所以这里：
//! - 配置无 provider 时把 env 端点合成一个 `env` provider 显示/拉取；
//! - 用户在列表里选定某个模型时，若该 provider 还没写进配置，则把 env 端点**落地**
//!   成真正的 provider 条目（ACP `session/update_config` 会校验 profile 引用的
//!   provider 必须存在，且 providers 不能为空——不落地则切换不会生效）。
//!
//! 拉取结果缓存在 [`crate::kit::atoms::MODEL_PANEL_REMOTE`]，`Ctrl+R` 手动刷新，
//! 打开面板时若缓存为空或过期（10 分钟）自动拉一次。

use crate::config::{AppConfig, PeriConfig, ProviderConfig, ProviderModels};
use std::time::{Duration, Instant};

/// 合成 provider 的 id（env 端点）
pub(crate) const ENV_PROVIDER_ID: &str = "env";
/// 缓存过期时间——超过则在下次打开面板时自动重拉
const STALE_AFTER: Duration = Duration::from_secs(600);

/// 拉取状态
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FetchStatus {
    /// 从未拉取
    #[default]
    Idle,
    Loading,
    Done,
    Failed(String),
}

/// 端点模型列表缓存（atom 值）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteModels {
    pub status: FetchStatus,
    /// (provider_id, model_id) —— 保留端点返回顺序
    pub entries: Vec<(String, String)>,
    pub fetched_at: Option<Instant>,
}

impl RemoteModels {
    pub(crate) fn is_stale(&self) -> bool {
        match self.status {
            FetchStatus::Idle => true,
            FetchStatus::Loading => false,
            FetchStatus::Failed(_) => false,
            FetchStatus::Done => self
                .fetched_at
                .is_none_or(|at| at.elapsed() > STALE_AFTER),
        }
    }
}

/// env 端点（镜像 `LlmProvider::from_env` 的读取顺序）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvEndpoint {
    /// "openai" | "anthropic"
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// 读取当前进程环境里的端点（无 API key 则视为未配置）
pub(crate) fn env_endpoint() -> Option<EnvEndpoint> {
    env_endpoint_with(|key| std::env::var(key).ok())
}

/// 可注入的 env 读取（测试用）：`get` 返回 None 等价于未设置。
pub(crate) fn env_endpoint_with(get: impl Fn(&str) -> Option<String>) -> Option<EnvEndpoint> {
    let hint = get("MODEL_PROVIDER").unwrap_or_default();
    let prefer_anthropic = hint.eq_ignore_ascii_case("anthropic")
        || (hint.is_empty() && get("ANTHROPIC_API_KEY").is_some());
    if prefer_anthropic {
        let api_key = get("ANTHROPIC_API_KEY")?;
        return Some(EnvEndpoint {
            provider_type: "anthropic".to_string(),
            base_url: get("ANTHROPIC_BASE_URL").unwrap_or_else(|| "https://api.anthropic.com".into()),
            api_key,
            model: get("ANTHROPIC_MODEL").unwrap_or_else(|| "claude-sonnet-4-6".into()),
        });
    }
    let api_key = get("OPENAI_API_KEY")?;
    let base_url = get("OPENAI_API_BASE")
        .or_else(|| get("OPENAI_BASE_URL"))
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    Some(EnvEndpoint {
        provider_type: "openai".to_string(),
        base_url,
        api_key,
        model: get("OPENAI_MODEL").unwrap_or_else(|| "gpt-4o".into()),
    })
}

/// 端点展示名：`env · host:port`
pub(crate) fn env_label(base_url: &str) -> String {
    let host = base_url
        .split("://")
        .last()
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or(base_url);
    if host.is_empty() {
        ENV_PROVIDER_ID.to_string()
    } else {
        format!("{ENV_PROVIDER_ID} · {host}")
    }
}

/// 配置里没有 provider 时，把 env 端点合成为一个可显示的 provider。
pub(crate) fn env_provider() -> Option<ProviderConfig> {
    let env = env_endpoint()?;
    Some(ProviderConfig {
        id: ENV_PROVIDER_ID.to_string(),
        provider_type: env.provider_type.clone(),
        api_key: env.api_key.clone(),
        base_url: env.base_url.clone(),
        name: Some(env_label(&env.base_url)),
        models: ProviderModels {
            fable: env.model.clone(),
            opus: env.model.clone(),
            sonnet: env.model.clone(),
            haiku: env.model,
        },
        extra: Default::default(),
    })
}

/// 列表/拉取共用的 provider 视图：配置 providers；为空时退化为 env 合成 provider。
pub(crate) fn effective_providers(app: &AppConfig) -> Vec<ProviderConfig> {
    if !app.providers.is_empty() {
        return app.providers.clone();
    }
    env_provider().into_iter().collect()
}

/// 落地 env 端点：把 env 端点写成配置里的 `env` provider（幂等）。
///
/// 返回落地后的 provider id；配置里已有 `env` 时直接返回它。
pub(crate) fn adopt_env_provider(app: &mut AppConfig) -> Option<String> {
    if app.providers.iter().any(|p| p.id == ENV_PROVIDER_ID) {
        return Some(ENV_PROVIDER_ID.to_string());
    }
    let provider = env_provider()?;
    app.providers.push(provider);
    Some(ENV_PROVIDER_ID.to_string())
}

/// 拉取目标：一个端点的 (provider_id, base_url, api_key)
fn fetch_targets(cfg: &PeriConfig) -> Vec<(String, String, String)> {
    let mut targets: Vec<(String, String, String)> = cfg
        .config
        .providers
        .iter()
        .filter(|p| !p.base_url.trim().is_empty())
        .map(|p| (p.id.clone(), p.base_url.clone(), p.api_key.clone()))
        .collect();
    if targets.is_empty()
        && let Some(env) = env_endpoint()
    {
        targets.push((ENV_PROVIDER_ID.to_string(), env.base_url, env.api_key));
    }
    targets
}

/// 打开面板时按需自动拉取（首次 / 过期）。
pub(crate) fn spawn_fetch_if_stale() {
    if crate::kit::atoms::MODEL_PANEL_REMOTE
        .state()
        .read()
        .is_stale()
    {
        spawn_fetch();
    }
}

/// 触发一次拉取（`Ctrl+R` / 自动）；已在加载中则忽略。
pub(crate) fn spawn_fetch() {
    {
        let state = crate::kit::atoms::MODEL_PANEL_REMOTE.state();
        let mut current = state.read().clone();
        if current.status == FetchStatus::Loading {
            return;
        }
        current.status = FetchStatus::Loading;
        *state.write() = current;
    }

    let cfg = crate::kit::atoms::PERI_CONFIG_HANDLE
        .get()
        .map(|handle| handle.read().clone());
    let targets = cfg.as_ref().map(fetch_targets).unwrap_or_default();
    if targets.is_empty() {
        let state = crate::kit::atoms::MODEL_PANEL_REMOTE.state();
        let mut current = state.read().clone();
        current.status = FetchStatus::Failed("no endpoint".to_string());
        *state.write() = current;
        return;
    }

    tokio::spawn(async move {
        let mut entries: Vec<(String, String)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build();
        match client {
            Ok(client) => {
                for (provider_id, base_url, api_key) in targets {
                    match fetch_models(&client, &base_url, &api_key).await {
                        Ok(models) => entries.extend(
                            models
                                .into_iter()
                                .map(|model| (provider_id.clone(), model)),
                        ),
                        Err(error) => errors.push(format!("{provider_id}: {error}")),
                    }
                }
            }
            Err(error) => errors.push(error.to_string()),
        }

        let state = crate::kit::atoms::MODEL_PANEL_REMOTE.state();
        let mut current = state.read().clone();
        if entries.is_empty() && !errors.is_empty() {
            current.status = FetchStatus::Failed(errors.join("; "));
        } else {
            current.status = FetchStatus::Done;
            current.entries = entries;
            current.fetched_at = Some(Instant::now());
        }
        *state.write() = current;
    });
}

async fn fetch_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = client.get(&url);
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response.text().await.map_err(|e| e.to_string())?;
    parse_models(&body)
}

/// 解析 `GET /models` 响应（OpenAI 兼容与 Anthropic 均为 `{"data":[{"id":…}]}`）。
pub(crate) fn parse_models(body: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let data = value
        .get("data")
        .and_then(|data| data.as_array())
        .ok_or_else(|| "response has no data[]".to_string())?;
    let mut models: Vec<String> = Vec::new();
    for item in data {
        let id = item
            .get("id")
            .and_then(|id| id.as_str())
            .or_else(|| item.as_str());
        if let Some(id) = id
            && !id.is_empty()
            && !models.iter().any(|model| model == id)
        {
            models.push(id.to_string());
        }
    }
    Ok(models)
}

/// 状态提示文案 + 是否错误色（渲染在面板底部提示行右侧）。
///
/// `None` = 无需提示（从未拉取 / 已拉取但结果为空）。
pub(crate) fn status_hint(state: &RemoteModels) -> Option<(String, bool)> {
    match &state.status {
        FetchStatus::Idle => None,
        FetchStatus::Loading => Some((
            crate::i18n::tr("panel-model-fetch-loading"),
            false,
        )),
        FetchStatus::Done => {
            let count = state.entries.len();
            if count == 0 {
                None
            } else {
                Some((
                    crate::i18n::tr_args(
                        "panel-model-fetch-done",
                        &[(
                            "count".to_string(),
                            fluent_bundle::FluentValue::from(count as i64),
                        )],
                    ),
                    false,
                ))
            }
        }
        FetchStatus::Failed(error) => Some((
            crate::i18n::tr_args(
                "panel-model-fetch-failed",
                &[(
                    "error".to_string(),
                    fluent_bundle::FluentValue::from(error.as_str()),
                )],
            ),
            true,
        )),
    }
}

#[cfg(test)]
#[path = "fetch_test.rs"]
mod tests;
