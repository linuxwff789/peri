//! `model/list.rs` 纯逻辑测试：列表构建 / 过滤 / 截断 / active 目标解析。

use super::*;
use crate::config::{AppConfig, PeriConfig, ProviderConfig, ProviderModels};

fn provider(id: &str, tiers: [&str; 4]) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        provider_type: "openai".to_string(),
        api_key: "key".to_string(),
        base_url: String::new(),
        name: None,
        models: ProviderModels {
            fable: tiers[0].to_string(),
            opus: tiers[1].to_string(),
            sonnet: tiers[2].to_string(),
            haiku: tiers[3].to_string(),
        },
        extra: Default::default(),
    }
}

fn config(providers: Vec<ProviderConfig>, active_alias: &str) -> PeriConfig {
    PeriConfig {
        schema: None,
        config: AppConfig {
            active_alias: active_alias.to_string(),
            providers,
            ..Default::default()
        },
    }
}

#[test]
fn build_choices_flattens_providers_in_config_order() {
    let cfg = config(
        vec![
            provider("alpha", ["a-fable", "a-opus", "a-sonnet", "a-haiku"]),
            provider("beta", ["", "b-opus", "", ""]),
        ],
        "opus",
    );

    let choices = build_choices(&cfg, "opus", &[]);
    let models: Vec<&str> = choices.iter().map(|c| c.model.as_str()).collect();
    assert_eq!(
        models,
        vec!["a-fable", "a-opus", "a-sonnet", "a-haiku", "b-opus"]
    );
    // 默认 Profile 未显式绑定 provider/model：解析回退到第一个 provider 的 opus 档。
    let current: Vec<&str> = choices
        .iter()
        .filter(|c| c.current)
        .map(|c| c.model.as_str())
        .collect();
    assert_eq!(current, vec!["a-opus"]);
    assert_eq!(choices[1].tiers, vec!["opus"]);
    assert_eq!(choices[0].tiers, vec!["fable"]);
}

#[test]
fn build_choices_dedupes_shared_model_across_tiers() {
    let cfg = config(
        vec![provider("alpha", ["same", "same", "other", "same"])],
        "opus",
    );
    let choices = build_choices(&cfg, "opus", &[]);
    let models: Vec<&str> = choices.iter().map(|c| c.model.as_str()).collect();
    assert_eq!(models, vec!["same", "other"]);
    assert_eq!(choices[0].tiers, vec!["fable", "opus", "haiku"]);
    assert_eq!(choices[1].tiers, vec!["sonnet"]);
}

#[test]
fn build_choices_marks_current_profile_target() {
    let mut cfg = config(vec![provider("alpha", ["f", "o", "s", "h"])], "sonnet");
    let profile = cfg.config.profiles.get_mut("sonnet").unwrap();
    profile.provider = "alpha".to_string();
    profile.model = Some("o".to_string());

    let choices = build_choices(&cfg, "sonnet", &[]);
    let current: Vec<&str> = choices
        .iter()
        .filter(|c| c.current)
        .map(|c| c.model.as_str())
        .collect();
    assert_eq!(current, vec!["o"]);
}

#[test]
fn build_choices_includes_manual_profile_model() {
    let mut cfg = config(vec![provider("alpha", ["f", "o", "s", "h"])], "haiku");
    let profile = cfg.config.profiles.get_mut("haiku").unwrap();
    profile.provider = "alpha".to_string();
    profile.model = Some("manual-1".to_string());

    let choices = build_choices(&cfg, "haiku", &[]);
    let models: Vec<&str> = choices.iter().map(|c| c.model.as_str()).collect();
    assert_eq!(models, vec!["f", "o", "s", "h", "manual-1"]);
    let manual = choices.last().unwrap();
    assert!(manual.tiers.is_empty());
    assert!(manual.current);
}

#[test]
fn build_choices_prepends_current_when_not_listed_anywhere() {
    // profile 指向不存在的 provider：解析回退到第一个 provider（与请求侧解析一致），
    // 手填 model 不在任何档位映射里 → 置顶补一条并标 ●。
    let mut cfg = config(vec![provider("alpha", ["f", "o", "s", "h"])], "opus");
    let profile = cfg.config.profiles.get_mut("opus").unwrap();
    profile.provider = "ghost".to_string();
    profile.model = Some("ghost-model".to_string());

    let choices = build_choices(&cfg, "opus", &[]);
    assert_eq!(choices[0].provider_id, "alpha");
    assert_eq!(choices[0].model, "ghost-model");
    assert!(choices[0].current);
    assert!(choices[0].tiers.is_empty());
}

#[test]
fn build_choices_prepends_current_when_provider_lists_no_model() {
    // provider 四个档位全空 + profile 手填 model：列表本为空，置顶补当前条目。
    let mut cfg = config(vec![provider("alpha", ["", "", "", ""])], "opus");
    let profile = cfg.config.profiles.get_mut("opus").unwrap();
    profile.model = Some("only-model".to_string());

    let choices = build_choices(&cfg, "opus", &[]);
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].provider_id, "alpha");
    assert_eq!(choices[0].model, "only-model");
    assert!(choices[0].current);
}

#[test]
fn filter_choices_empty_query_returns_all() {
    let cfg = config(vec![provider("alpha", ["f", "o", "s", "h"])], "opus");
    let choices = build_choices(&cfg, "opus", &[]);
    assert_eq!(filter_choices(&choices, ""), vec![0, 1, 2, 3]);
    assert_eq!(filter_choices(&choices, "   "), vec![0, 1, 2, 3]);
}

#[test]
fn filter_choices_matches_model_provider_and_label() {
    let cfg = config(
        vec![
            provider("alpha", ["gpt-4o", "gpt-4o-mini", "", ""]),
            provider("beta", ["", "claude-3", "", ""]),
        ],
        "opus",
    );
    let choices = build_choices(&cfg, "opus", &[]);

    assert_eq!(filter_choices(&choices, "gpt"), vec![0, 1]);
    assert_eq!(filter_choices(&choices, "ALPHA"), vec![0, 1]);
    assert_eq!(filter_choices(&choices, "beta claude"), vec![2]);
    assert_eq!(filter_choices(&choices, "alpha claude"), Vec::<usize>::new());
    assert_eq!(filter_choices(&choices, "4o mini"), vec![1]);
}

#[test]
fn active_target_falls_back_to_first_provider_and_alias() {
    let cfg = config(vec![provider("alpha", ["f", "o", "s", "h"])], "opus");
    // Profile.provider 为空 → 第一个 provider；Profile.model 未设置 →
    // provider 同档位映射（opus → "o"）。
    assert_eq!(
        active_target(&cfg.config, "opus"),
        ("alpha".to_string(), "o".to_string())
    );
}

#[test]
fn truncate_to_respects_display_width() {
    assert_eq!(truncate_to("short", 10), "short");
    assert_eq!(truncate_to("abcdef", 4), "abc…");
    // 宽字符按显示宽度计算
    assert_eq!(truncate_to("中文模型", 4), "中…");
}

#[test]
fn build_choices_appends_remote_models_per_provider() {
    let cfg = config(vec![provider("alpha", ["f", "o", "s", "h"])], "opus");
    let remote = vec![
        ("alpha".to_string(), "remote-1".to_string()),
        ("alpha".to_string(), "o".to_string()), // 已在档位映射里 → 不重复
        ("beta".to_string(), "ghost-provider".to_string()), // provider 不存在 → 丢弃
    ];
    let choices = build_choices(&cfg, "opus", &remote);
    let models: Vec<&str> = choices.iter().map(|c| c.model.as_str()).collect();
    assert_eq!(models, vec!["f", "o", "s", "h", "remote-1"]);
    assert!(choices[4].tiers.is_empty(), "端点模型没有档位徽标");
    assert!(!choices[4].current);
}
