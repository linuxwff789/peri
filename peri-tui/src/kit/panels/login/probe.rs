//! Login 面板的端点探测（pi 式「填完 endpoint 就自动出模型」）。
//!
//! 三件事：
//! 1. **URL 归一化**：用户常直接粘贴完整 chat 地址（`…/v1/chat/completions`）或只填
//!    host（`http://host:8787`）。peri 的 openai transport 会自己拼 `/chat/completions`，
//!    所以要剥掉 API 路径后缀、给只填 host 的补 `/v1`，否则请求地址会拼错。
//! 2. **API Key 回退 env**：`LlmProvider::from_config_for_alias` 要求 `api_key` 非空，
//!    空 key 的 provider 直接不可用（"active profile has no usable provider"）；
//!    登录表单留空时回退 `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`。
//! 3. **探测 `/models` 并落地**：成功后把模型列表写进 provider 的 `extra["models_list"]`
//!    （`/model` 直接可见，离线也在），四个档位全空时用第一个模型占位；同时并入
//!    `MODEL_PANEL_REMOTE` 缓存并弹通知。

use crate::config::{PeriConfig, ProviderConfig};
use crate::i18n;
use crate::kit::atoms::{
    ACP_CLIENT_HANDLE, LOGIN_PROBE, MODEL_PANEL_REMOTE, NOTIFICATION, Notification,
    PERI_CONFIG_HANDLE,
};
use crate::kit::panels::login::config_store::refresh_provider_list;
use crate::kit::panels::model::fetch::{FetchStatus, parse_models};
use fluent_bundle::FluentValue;
use std::time::{Duration, Instant};

/// provider.extra 里持久化模型列表的 key（`/model` 离线也能列出）
pub(crate) const MODELS_EXTRA_KEY: &str = "models_list";

/// OpenAI / Anthropic 的 API 路径后缀——粘贴完整地址时剥掉，只留 base。
const API_PATH_SUFFIXES: [&str; 7] = [
    "/chat/completions",
    "/completions",
    "/responses",
    "/messages",
    "/models",
    "/embeddings",
    "/v1/messages",
];

/// 探测状态
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProbeStatus {
    #[default]
    Idle,
    Loading,
    Done(usize),
    Failed(String),
}

/// 探测状态 + 目标 provider（atom 值）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeState {
    pub provider_id: String,
    pub status: ProbeStatus,
}

/// base URL 归一化（纯函数）：
/// - 无 scheme 补 `http://`；去掉 query/fragment 与尾部 `/`；
/// - 剥掉 API 路径后缀（`/chat/completions` 等）；
/// - 只剩 host（无 path）时补 `/v1`。
pub(crate) fn normalize_base_url(raw: &str) -> String {
    let mut url = raw.trim().to_string();
    if url.is_empty() {
        return url;
    }
    if !url.contains("://") {
        url = format!("http://{url}");
    }
    for sep in ['?', '#'] {
        if let Some(pos) = url.find(sep) {
            url.truncate(pos);
        }
    }
    while url.ends_with('/') {
        url.pop();
    }
    let lower = url.to_lowercase();
    for suffix in API_PATH_SUFFIXES {
        if lower.ends_with(suffix) {
            url.truncate(url.len() - suffix.len());
            while url.ends_with('/') {
                url.pop();
            }
            break;
        }
    }
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url.as_str());
    if !after_scheme.contains('/') {
        url.push_str("/v1");
    }
    url
}

/// 从 env 读 provider 对应的 API key（可注入 `get` 便于测试）。
pub(crate) fn env_api_key(
    provider_type: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let var = if provider_type == "anthropic" {
        "ANTHROPIC_API_KEY"
    } else {
        "OPENAI_API_KEY"
    };
    get(var)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 从 env 读 provider 对应的默认模型（`OPENAI_MODEL` / `ANTHROPIC_MODEL`）。
///
/// 探测后拿它做档位占位——不会把用户当前在用的模型悄悄换掉。
pub(crate) fn env_model(
    provider_type: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let var = if provider_type == "anthropic" {
        "ANTHROPIC_MODEL"
    } else {
        "OPENAI_MODEL"
    };
    get(var)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 表单填的 key 优先；空则回退 env。
pub(crate) fn resolve_api_key(provider_type: &str, configured: &str) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    env_api_key(provider_type, |key| std::env::var(key).ok()).unwrap_or_default()
}

/// 读取持久化的模型列表（`extra["models_list"]`）。
pub(crate) fn stored_models(provider: &ProviderConfig) -> Vec<String> {
    provider
        .extra
        .get(MODELS_EXTRA_KEY)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|model| model.trim().to_string())
                .filter(|model| !model.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 把探测到的模型写进配置（纯函数）：`extra["models_list"]` + 四个档位全空时用
/// `preferred`（env 里正在用的模型，必须在列表内）或列表第一个模型占位——
/// 否则 `from_config` 解析不出模型，ACP 会拒。返回是否有变化。
pub(crate) fn apply_models_to_config(
    cfg: &mut PeriConfig,
    provider_id: &str,
    models: &[String],
    preferred: Option<&str>,
) -> bool {
    let Some(provider) = cfg
        .config
        .providers
        .iter_mut()
        .find(|p| p.id == provider_id)
    else {
        return false;
    };
    let mut changed = false;
    let value = serde_json::json!(models);
    if provider.extra.get(MODELS_EXTRA_KEY) != Some(&value) {
        provider.extra.insert(MODELS_EXTRA_KEY.to_string(), value);
        changed = true;
    }
    let placeholder = preferred
        .filter(|model| models.iter().any(|item| item == model))
        .or_else(|| models.first().map(String::as_str))
        .unwrap_or("");
    if !placeholder.is_empty() {
        if provider.models.fable.trim().is_empty() {
            provider.models.fable = placeholder.to_string();
            changed = true;
        }
        if provider.models.opus.trim().is_empty() {
            provider.models.opus = placeholder.to_string();
            changed = true;
        }
        if provider.models.sonnet.trim().is_empty() {
            provider.models.sonnet = placeholder.to_string();
            changed = true;
        }
        if provider.models.haiku.trim().is_empty() {
            provider.models.haiku = placeholder.to_string();
            changed = true;
        }
    }
    changed
}

/// 探测结果并入 `/model` 的远端缓存（该 provider 的旧条目先清掉）。
pub(crate) fn merge_remote_entries(provider_id: &str, models: &[String]) {
    let state = MODEL_PANEL_REMOTE.state();
    let mut current = state.read().clone();
    current.entries.retain(|(pid, _)| pid != provider_id);
    current.entries.extend(
        models
            .iter()
            .map(|model| (provider_id.to_string(), model.clone())),
    );
    if !models.is_empty() {
        current.status = FetchStatus::Done;
        current.fetched_at = Some(Instant::now());
    }
    *state.write() = current;
}

fn set_probe_status(provider_id: &str, status: ProbeStatus) {
    *LOGIN_PROBE.state().write() = ProbeState {
        provider_id: provider_id.to_string(),
        status,
    };
}

/// 用「表单当前值」探测（编辑模式 `Ctrl+R`，不落盘）。
pub(crate) fn spawn_probe_from_form(
    provider_id: &str,
    base_url: &str,
    api_key: &str,
    provider_type: &str,
) {
    let key = resolve_api_key(provider_type, api_key);
    spawn_probe(
        provider_id.to_string(),
        base_url.to_string(),
        key,
        provider_type.to_string(),
    );
}

/// 用「已保存的配置」探测（浏览模式 `Ctrl+R` / 保存后自动）。
pub(crate) fn spawn_probe_for_saved(provider_id: &str) {
    let Some(handle) = PERI_CONFIG_HANDLE.get() else {
        return;
    };
    let target = {
        let cfg = handle.read();
        cfg.config
            .providers
            .iter()
            .find(|p| p.id == provider_id)
            .map(|p| {
                (
                    p.base_url.clone(),
                    resolve_api_key(&p.provider_type, &p.api_key),
                    p.provider_type.clone(),
                )
            })
    };
    let Some((base_url, api_key, provider_type)) = target else {
        return;
    };
    spawn_probe(provider_id.to_string(), base_url, api_key, provider_type);
}

/// 发起探测：`GET {base}/models` → 落地配置 + 缓存 + 通知 + 状态。
pub(crate) fn spawn_probe(
    provider_id: String,
    base_url: String,
    api_key: String,
    provider_type: String,
) {
    let normalized = normalize_base_url(&base_url);
    if normalized.is_empty() {
        set_probe_status(&provider_id, ProbeStatus::Failed(i18n::tr("login-probe-no-url")));
        return;
    }
    set_probe_status(&provider_id, ProbeStatus::Loading);

    tokio::spawn(async move {
        let result = match reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
        {
            Ok(client) => fetch_models(&client, &normalized, &api_key, &provider_type).await,
            Err(error) => Err(error.to_string()),
        };
        match result {
            Ok(models) => {
                commit_models(&provider_id, &normalized, &api_key, &provider_type, &models);
                set_probe_status(&provider_id, ProbeStatus::Done(models.len()));
            }
            Err(error) => {
                tracing::warn!(provider_id = %provider_id, %error, "LoginPanel: probe failed");
                set_probe_status(&provider_id, ProbeStatus::Failed(error));
            }
        }
    });
}

/// 落地探测结果：配置（extra + 空档位 + 归一化 baseUrl + 空 key 补 env key）→
/// 持久化 → ACP → 缓存 → 通知。
///
/// `LlmProvider::from_config_for_alias` 要求 `api_key` 非空，所以探测用的 env key
/// 成功时也写回配置——否则 provider 仍然不可用（ACP 会拒 "no usable provider"）。
fn commit_models(
    provider_id: &str,
    base_url: &str,
    api_key: &str,
    provider_type: &str,
    models: &[String],
) {
    let preferred = env_model(provider_type, |key| std::env::var(key).ok());
    if let Some(handle) = PERI_CONFIG_HANDLE.get() {
        let (snap, changed) = {
            let mut cfg = handle.write();
            let mut changed = apply_models_to_config(
                &mut cfg,
                provider_id,
                models,
                preferred.as_deref(),
            );
            if let Some(provider) = cfg.config.providers.iter_mut().find(|p| p.id == provider_id) {
                if !base_url.is_empty() && provider.base_url != base_url {
                    provider.base_url = base_url.to_string();
                    changed = true;
                }
                if !api_key.is_empty() && provider.api_key.trim().is_empty() {
                    provider.api_key = api_key.to_string();
                    changed = true;
                }
            }
            (cfg.clone(), changed)
        };
        if changed {
            match crate::config::save_effective(&snap) {
                Ok(()) => {
                    refresh_provider_list();
                    if let Some(client) = ACP_CLIENT_HANDLE.get() {
                        tokio::spawn(async move {
                            if let Err(error) = client.update_config(&snap).await {
                                tracing::warn!(%error, "LoginPanel: update_config push failed after probe");
                            }
                        });
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "LoginPanel: persist after probe failed");
                }
            }
        }
    }

    merge_remote_entries(provider_id, models);

    *NOTIFICATION.state().write() = Some(Notification {
        message: i18n::tr_args(
            "login-probe-done",
            &[(
                "count".to_string(),
                FluentValue::from(models.len() as i64),
            )],
        ),
        until: Instant::now() + Duration::from_secs(3),
    });
}

async fn fetch_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    provider_type: &str,
) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = client.get(&url);
    if !api_key.is_empty() {
        request = if provider_type == "anthropic" {
            request
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
        } else {
            request.bearer_auth(api_key)
        };
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response.text().await.map_err(|e| e.to_string())?;
    parse_models(&body)
}

#[cfg(test)]
#[path = "probe_test.rs"]
mod tests;
