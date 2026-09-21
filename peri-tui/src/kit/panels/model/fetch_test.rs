//! `model/fetch.rs` 纯逻辑测试：env 端点解析 / /models 响应解析 / env provider 落地。

use super::*;
use crate::config::AppConfig;
use std::collections::HashMap;

fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn with_env(pairs: &[(&str, &str)]) -> Option<EnvEndpoint> {
    let map = env_map(pairs);
    env_endpoint_with(|key| map.get(key).cloned())
}

#[test]
fn env_endpoint_openai_defaults() {
    let env = with_env(&[
        ("OPENAI_API_KEY", "sk-x"),
        ("OPENAI_BASE_URL", "http://34.81.77.240:8787/v1"),
        ("OPENAI_MODEL", "bai/glm-5.3-flash"),
    ])
    .expect("env endpoint");
    assert_eq!(env.provider_type, "openai");
    assert_eq!(env.base_url, "http://34.81.77.240:8787/v1");
    assert_eq!(env.model, "bai/glm-5.3-flash");
}

#[test]
fn env_endpoint_requires_api_key() {
    assert!(with_env(&[("OPENAI_BASE_URL", "http://x/v1")]).is_none());
}

#[test]
fn env_endpoint_anthropic_when_hinted_or_key_present() {
    let hinted = with_env(&[
        ("MODEL_PROVIDER", "anthropic"),
        ("ANTHROPIC_API_KEY", "sk-a"),
        ("ANTHROPIC_MODEL", "claude-sonnet-4-6"),
    ])
    .expect("anthropic");
    assert_eq!(hinted.provider_type, "anthropic");
    assert_eq!(hinted.base_url, "https://api.anthropic.com");

    // 无 MODEL_PROVIDER 但有 ANTHROPIC_API_KEY → 与 LlmProvider::from_env 一样优先 anthropic
    let implicit = with_env(&[("ANTHROPIC_API_KEY", "sk-a"), ("OPENAI_API_KEY", "sk-o")])
        .expect("anthropic");
    assert_eq!(implicit.provider_type, "anthropic");
}

#[test]
fn env_label_uses_host_of_base_url() {
    assert_eq!(
        env_label("http://34.81.77.240:8787/v1"),
        "env · 34.81.77.240:8787"
    );
    assert_eq!(env_label("https://api.openai.com/v1/"), "env · api.openai.com");
}

#[test]
fn parse_models_reads_data_ids() {
    let body = r#"{"object":"list","data":[{"id":"a"},{"id":"b"},{"id":"a"},{"id":""}]}"#;
    assert_eq!(parse_models(body).unwrap(), vec!["a", "b"]);
}

#[test]
fn parse_models_accepts_bare_strings_and_rejects_garbage() {
    assert_eq!(
        parse_models(r#"{"data":["m1","m2"]}"#).unwrap(),
        vec!["m1", "m2"]
    );
    assert!(parse_models("not json").is_err());
    assert!(parse_models(r#"{"object":"list"}"#).is_err());
}

#[test]
fn adopt_env_provider_is_idempotent_when_already_present() {
    let mut app = AppConfig::default();
    app.providers.push(ProviderConfig {
        id: ENV_PROVIDER_ID.to_string(),
        ..Default::default()
    });
    assert_eq!(adopt_env_provider(&mut app), Some(ENV_PROVIDER_ID.to_string()));
    assert_eq!(app.providers.len(), 1, "已存在时不重复插入");
}

#[test]
fn effective_providers_falls_back_to_env() {
    let app = AppConfig::default();
    // 本进程没有 OPENAI_API_KEY 时 env 合成 provider 为空 → 列表也为空
    let providers = effective_providers(&app);
    if std::env::var("OPENAI_API_KEY").is_err() && std::env::var("ANTHROPIC_API_KEY").is_err() {
        assert!(providers.is_empty());
    }
}

#[test]
fn remote_models_stale_rules() {
    let mut state = RemoteModels::default();
    assert!(state.is_stale(), "Idle 视为过期（打开面板自动拉取）");

    state.status = FetchStatus::Loading;
    assert!(!state.is_stale(), "加载中不重复触发");

    state.status = FetchStatus::Done;
    state.fetched_at = Some(Instant::now());
    assert!(!state.is_stale());

    state.fetched_at = Some(Instant::now() - STALE_AFTER - Duration::from_secs(1));
    assert!(state.is_stale());

    state.status = FetchStatus::Failed("boom".into());
    assert!(!state.is_stale(), "失败后不自动重试，等用户 Ctrl+R");
}
