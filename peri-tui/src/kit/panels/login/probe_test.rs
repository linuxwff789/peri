//! `login/probe.rs` 纯逻辑测试：URL 归一化 / api key 回退 / 模型落地。

use super::*;
use crate::config::{AppConfig, PeriConfig, ProviderConfig, ProviderModels};
use std::collections::HashMap;

#[test]
fn normalize_adds_scheme_and_v1_for_bare_host() {
    assert_eq!(
        normalize_base_url("34.81.77.240:8787"),
        "http://34.81.77.240:8787/v1"
    );
    assert_eq!(normalize_base_url("http://host:8787"), "http://host:8787/v1");
    assert_eq!(
        normalize_base_url("https://api.openai.com"),
        "https://api.openai.com/v1"
    );
}

#[test]
fn normalize_strips_api_paths() {
    // 用户常直接粘贴完整 chat 地址——peri 的 transport 自己拼 /chat/completions
    assert_eq!(
        normalize_base_url("http://host:8787/v1/chat/completions"),
        "http://host:8787/v1"
    );
    assert_eq!(
        normalize_base_url("http://host:8787/v1/chat/completions/"),
        "http://host:8787/v1"
    );
    assert_eq!(
        normalize_base_url("https://api.anthropic.com/v1/messages"),
        "https://api.anthropic.com"
    );
    assert_eq!(
        normalize_base_url("https://host/v1/models"),
        "https://host/v1"
    );
    assert_eq!(
        normalize_base_url("https://host/openai/responses"),
        "https://host/openai"
    );
}

#[test]
fn normalize_keeps_custom_base_path() {
    // 自定义路径（Azure 风格）不补 /v1，也不动
    assert_eq!(
        normalize_base_url("https://host/custom/openai/v1"),
        "https://host/custom/openai/v1"
    );
    assert_eq!(normalize_base_url(""), "");
    assert_eq!(normalize_base_url("   "), "");
    assert_eq!(
        normalize_base_url("http://host:8787/v1?x=1"),
        "http://host:8787/v1"
    );
}

#[test]
fn env_api_key_picks_by_provider_type() {
    let map: HashMap<&str, &str> = [("OPENAI_API_KEY", "sk-openai"), ("ANTHROPIC_API_KEY", "sk-ant")]
        .into_iter()
        .collect();
    let get = |key: &str| map.get(key).map(|value| value.to_string());
    assert_eq!(
        env_api_key("openai", get).as_deref(),
        Some("sk-openai")
    );
    assert_eq!(env_api_key("anthropic", get).as_deref(), Some("sk-ant"));
    assert_eq!(env_api_key("openai", |_| None), None);
    assert_eq!(
        env_api_key("openai", |_| Some("  ".to_string())),
        None,
        "空白值不算配置"
    );
}

#[test]
fn resolve_api_key_prefers_form_value() {
    assert_eq!(resolve_api_key("openai", " sk-form "), "sk-form");
    // 表单为空时回退 env（测试进程通常没有这两个变量 → 空串）
    let from_env = resolve_api_key("openai", "");
    assert_eq!(from_env, std::env::var("OPENAI_API_KEY").unwrap_or_default());
}

#[test]
fn stored_models_reads_extra_list() {
    let provider = ProviderConfig {
        id: "p".to_string(),
        extra: [(
            MODELS_EXTRA_KEY.to_string(),
            serde_json::json!(["a", "", "b", "a"]),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    assert_eq!(stored_models(&provider), vec!["a", "b", "a"]);
    assert!(stored_models(&ProviderConfig::default()).is_empty());
}

#[test]
fn apply_models_fills_extra_and_empty_tiers_only() {
    let mut cfg = PeriConfig {
        schema: None,
        config: AppConfig {
            providers: vec![ProviderConfig {
                id: "gproxy".to_string(),
                models: ProviderModels {
                    // opus 已有用户手填的模型 → 不能被覆盖
                    opus: "keep-me".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        },
    };
    let models = vec!["m1".to_string(), "m2".to_string()];
    assert!(apply_models_to_config(&mut cfg, "gproxy", &models));
    let provider = &cfg.config.providers[0];
    assert_eq!(stored_models(provider), models);
    assert_eq!(provider.models.opus, "keep-me");
    assert_eq!(provider.models.fable, "m1");
    assert_eq!(provider.models.sonnet, "m1");
    assert_eq!(provider.models.haiku, "m1");
    // 幂等：同样的列表再来一次不再算变化
    assert!(!apply_models_to_config(&mut cfg, "gproxy", &models));
    // 未知 provider 不报错也不变
    assert!(!apply_models_to_config(&mut cfg, "ghost", &models));
}
