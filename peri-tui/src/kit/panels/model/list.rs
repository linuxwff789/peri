//! ModelPanel 的 pi 式扁平模型列表（默认视图）。
//!
//! 与 pi 的 `/model` 选择器对齐：
//! - 所有 provider × model 平铺成一个列表（不再先选档位再选 provider/model）；
//! - 直接打字即过滤（大小写不敏感子串，空格分词 AND），`⌫` 删除、`Ctrl+U` 清空；
//! - `↑/↓` 选择、`PgUp/PgDn` 翻页、`Enter` 立即切换并关闭；
//! - 鼠标 hover 跟随、点击即切换；
//! - `Tab` 进入档位编辑器（fable/opus/sonnet/haiku 四档 → provider/model/effort…），
//!   编辑器内 `Esc` 回到本列表。
//!
//! 本模块只放纯逻辑与事件处理——组件骨架（hooks/布局/panel_shell）留在 `model.rs`。

use super::edit::apply_model_choice;
use super::fetch::{effective_providers, spawn_fetch};
use crate::config::{AppConfig, PeriConfig, ProviderConfig, Profiles};
use crate::i18n;
use crate::kit::list_nav::{next_selection, previous_selection};
use crate::kit::panel_mouse::{hit_row, ListLayout};
use crate::kit::panel_registry::close_active_panel;
use peri_theme::theme::ThemeDefinition;
use ratatui_kit::crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui_kit::prelude::{EventResult, State};
use ratatui_kit::ratatui::layout::Rect;
use ratatui_kit::ratatui::style::Style;
use ratatui_kit::ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// 面板视图：pi 式扁平列表 / 原四档 profile 编辑器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ModelPanelView {
    #[default]
    List,
    Tiers,
}

/// 档位展示顺序（与 provider.models 字段声明序一致）
pub(super) const TIER_ORDER: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// 列表行：一个可选的 provider × model 组合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub provider_id: String,
    pub provider_label: String,
    pub model: String,
    /// 该模型在 provider 档位映射里占用的档位（用于行尾徽标）；手填 model 可为空。
    pub tiers: Vec<&'static str>,
    /// 是否是当前 active 档位正在使用的组合（行首 ● 标记）。
    pub current: bool,
}

/// provider 档位原始字段——不用 `get_model`（fable 空会回退 opus，导致档位徽标重复）。
fn raw_tier<'a>(p: &'a ProviderConfig, tier: &str) -> &'a str {
    match tier {
        "fable" => &p.models.fable,
        "opus" => &p.models.opus,
        "sonnet" => &p.models.sonnet,
        "haiku" => &p.models.haiku,
        _ => "",
    }
}

/// active 档位名：空串（全新配置 / 从未写盘）回退 `"opus"`。
///
/// `AppConfig::default()` 的 `active_alias` 是空串（serde 的 default 只在反序列化时生效），
/// 而 profile 查找只认四个档位名——不归一化的话面板会变成只读（改什么都不落盘）。
pub(crate) fn effective_alias(app: &AppConfig) -> String {
    if app.active_alias.trim().is_empty() {
        "opus".to_string()
    } else {
        app.active_alias.clone()
    }
}

/// active 档位解析出的 (provider_id, model)。
///
/// `providers` 用「有效 provider 列表」（配置 providers，为空时含 env 合成 provider）——
/// 否则 env 场景下 active 会解析成空 id，列表里的 ● 会落在兜底行而不是真正在用的模型。
/// 解析规则与 `edit.rs` 一致：Profile.provider 为空 → 第一个 provider；
/// Profile.model > provider.models 同档位映射 > alias 名。
pub(crate) fn active_target(
    app: &AppConfig,
    alias: &str,
    providers: &[ProviderConfig],
) -> (String, String) {
    let profile = app.profiles.get(alias);
    let provider = profile
        .and_then(|pf| {
            if pf.provider.is_empty() {
                providers.first()
            } else {
                providers.iter().find(|p| p.id == pf.provider)
            }
        })
        .or_else(|| providers.first());
    let provider_id = provider.map(|p| p.id.clone()).unwrap_or_default();
    let model = profile
        .and_then(|pf| pf.model.clone().filter(|m| !m.is_empty()))
        .or_else(|| provider.and_then(|p| p.models.get_model(alias)).map(str::to_string))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| alias.to_string());
    (provider_id, model)
}

/// 从配置构建扁平列表（provider 顺序 = 配置顺序；provider 内 = 档位顺序）。
///
/// 每个 provider 的候选模型 = 四个档位字段（去空去重）+ 绑定到该 provider 的
/// profile 手填 model（去重）+ 端点拉取的模型（`remote`，按返回序去重追加）。
/// 配置里没有 provider 时（env 起 peri）退化为合成的 `env` provider；
/// active 目标不在其中时置顶补一条，保证 ● 恒可见。
pub(crate) fn build_choices(
    cfg: &PeriConfig,
    active_alias: &str,
    remote: &[(String, String)],
) -> Vec<ModelChoice> {
    let app = &cfg.config;
    let active_alias = if active_alias.trim().is_empty() {
        effective_alias(app)
    } else {
        active_alias.to_string()
    };
    let providers = effective_providers(app);
    let (active_pid, active_model) = active_target(app, &active_alias, &providers);
    let mut out: Vec<ModelChoice> = Vec::new();

    for provider in &providers {
        let mut models: Vec<String> = Vec::new();
        for tier in TIER_ORDER {
            let model = raw_tier(provider, tier);
            if !model.is_empty() && !models.iter().any(|m| m == model) {
                models.push(model.to_string());
            }
        }
        for alias in Profiles::ALL {
            let Some(profile) = app.profiles.get(alias) else {
                continue;
            };
            if profile.provider != provider.id {
                continue;
            }
            if let Some(model) = profile.model.as_deref().filter(|m| !m.is_empty())
                && !models.iter().any(|m| m == model)
            {
                models.push(model.to_string());
            }
        }
        for (provider_id, model) in remote {
            if provider_id == &provider.id && !models.iter().any(|m| m == model) {
                models.push(model.clone());
            }
        }
        for model in models {
            let tiers: Vec<&'static str> = TIER_ORDER
                .iter()
                .copied()
                .filter(|tier| {
                    let raw = raw_tier(provider, tier);
                    !raw.is_empty() && raw == model
                })
                .collect();
            let current = provider.id == active_pid && model == active_model;
            out.push(ModelChoice {
                provider_id: provider.id.clone(),
                provider_label: provider.display_name().to_string(),
                model,
                tiers,
                current,
            });
        }
    }

    if !out.iter().any(|c| c.current) && !active_model.is_empty() {
        let label = providers
            .iter()
            .find(|p| p.id == active_pid)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| active_pid.clone());
        out.insert(
            0,
            ModelChoice {
                provider_id: active_pid,
                provider_label: label,
                model: active_model,
                tiers: Vec::new(),
                current: true,
            },
        );
    }
    out
}

/// 按查询串过滤，返回命中项在全量列表中的下标。
///
/// 空查询返回全部；否则空格分词后**每个** token 都要命中
/// `provider/model` 或 provider 显示名（大小写不敏感子串）。
pub(crate) fn filter_choices(choices: &[ModelChoice], query: &str) -> Vec<usize> {
    let normalized = query.trim().to_lowercase();
    if normalized.is_empty() {
        return (0..choices.len()).collect();
    }
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    choices
        .iter()
        .enumerate()
        .filter(|(_, choice)| {
            let haystack = format!("{}/{}", choice.provider_id, choice.model).to_lowercase();
            let label = choice.provider_label.to_lowercase();
            tokens
                .iter()
                .all(|token| haystack.contains(token) || label.contains(token))
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// 行宽不足时按显示宽度截断并加省略号。
fn truncate_to(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    format!("{out}…")
}

/// 渲染可见行（已按 scroll_start/visible 裁剪）。
///
/// 行布局：`❯ ● model                 fable·opus  provider`（右端对齐到 width）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_rows(
    choices: &[ModelChoice],
    indices: &[usize],
    selected: usize,
    scroll_start: usize,
    visible: usize,
    width: usize,
    theme: &ThemeDefinition,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if indices.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", i18n::tr("panel-model-list-empty")),
            Style::new().fg(theme.semantic.text.dim),
        )));
        return lines;
    }

    for (visual_idx, &choice_idx) in indices
        .iter()
        .enumerate()
        .skip(scroll_start)
        .take(visible)
    {
        let choice = &choices[choice_idx];
        let is_selected = visual_idx == selected;
        let mark = if is_selected { "❯" } else { " " };
        let current = if choice.current { "●" } else { " " };
        let prefix = format!(" {mark} {current} ");
        let prefix_width = UnicodeWidthStr::width(prefix.as_str());
        let badges = choice.tiers.join("·");
        let right = if badges.is_empty() {
            choice.provider_label.clone()
        } else {
            format!("{badges}  {}", choice.provider_label)
        };
        let right_width = UnicodeWidthStr::width(right.as_str());
        let model_width = width
            .saturating_sub(prefix_width + right_width + 1)
            .max(4);
        let model = truncate_to(&choice.model, model_width);
        let model_width = UnicodeWidthStr::width(model.as_str());
        let pad = width
            .saturating_sub(prefix_width + model_width + right_width)
            .max(1);

        let model_style = if is_selected {
            Style::new().fg(theme.semantic.accent).bold()
        } else {
            Style::new().fg(theme.semantic.model_info)
        };
        let prefix_style = if choice.current {
            Style::new().fg(theme.semantic.status.success).bold()
        } else if is_selected {
            Style::new().fg(theme.semantic.accent).bold()
        } else {
            Style::new().fg(theme.semantic.text.muted)
        };
        let mut spans = vec![
            Span::styled(prefix, prefix_style),
            Span::styled(model, model_style),
            Span::raw(" ".repeat(pad)),
        ];
        if !badges.is_empty() {
            spans.push(Span::styled(
                badges,
                Style::new().fg(theme.semantic.token_context),
            ));
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            choice.provider_label.clone(),
            Style::new().fg(theme.semantic.text.dim),
        ));
        lines.push(Line::from(spans));
    }
    lines
}

/// 搜索框行（`⌕ 输入…` + 查询串）。
pub(crate) fn search_line(
    query: &str,
    theme: &ThemeDefinition,
    focused: bool,
) -> Line<'static> {
    let placeholder = if query.is_empty() {
        i18n::tr("panel-model-list-placeholder")
    } else {
        String::new()
    };
    let cursor_style = if focused {
        Style::new().fg(theme.semantic.accent).bold()
    } else {
        Style::new().fg(theme.semantic.text.dim)
    };
    let mut spans = vec![
        Span::styled("  ⌕ ", Style::new().fg(theme.semantic.text.muted)),
        Span::styled(
            if query.is_empty() {
                placeholder
            } else {
                query.to_string()
            },
            if query.is_empty() {
                Style::new().fg(theme.semantic.text.dim)
            } else {
                Style::new().fg(theme.semantic.text.primary)
            },
        ),
    ];
    if focused {
        spans.push(Span::styled("▏", cursor_style));
    }
    Line::from(spans)
}

/// 列表视图事件（键盘 + 鼠标）。
///
/// 选择下标 `selected` 是**过滤后**列表的下标；命中数据每帧从配置重算，
/// 避免把易失的派生快照跨帧持有。
pub(super) fn handle_list_event(
    event: Event,
    area: Option<Rect>,
    view: State<ModelPanelView>,
    query: State<String>,
    selected: State<usize>,
    visible: usize,
) -> EventResult {
    let (indices, choices) = filtered_snapshot(&query.read());

    if let Event::Mouse(mouse) = event {
        let Some(area) = area else {
            return EventResult::Ignored;
        };
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(idx) = hit_row(
                    mouse.row,
                    area,
                    list_layout(&indices, *selected.read(), visible),
                ) && *selected.read() != idx
                {
                    *selected.write() = idx;
                }
                EventResult::Consumed
            }
            MouseEventKind::Down(MouseButton::Left) => {
                match hit_row(
                    mouse.row,
                    area,
                    list_layout(&indices, *selected.read(), visible),
                ) {
                    Some(idx) => {
                        *selected.write() = idx;
                        apply_selected(&choices, &indices, idx);
                        close_active_panel();
                        EventResult::Consumed
                    }
                    // 面板内空白/边框点击：消费不穿透（与其它面板一致）
                    None => EventResult::Consumed,
                }
            }
            _ => EventResult::Ignored,
        }
    } else {
        let Event::Key(key) = event else {
            return EventResult::Ignored;
        };
        if key.kind != KeyEventKind::Press {
            return EventResult::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                query.write().clear();
                *selected.write() = 0;
                EventResult::Consumed
            }
            // Ctrl+R：重新拉取端点模型列表（`r` 本身是搜索字符，所以必须带 Ctrl）。
            (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
                spawn_fetch();
                EventResult::Consumed
            }
            (mods, KeyCode::Char(ch))
                if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) =>
            {
                query.write().push(ch);
                *selected.write() = 0;
                EventResult::Consumed
            }
            (_, KeyCode::Backspace) if !alt => {
                query.write().pop();
                *selected.write() = 0;
                EventResult::Consumed
            }
            (KeyModifiers::NONE, KeyCode::Up) if !ctrl => {
                let mut sel = selected.write();
                *sel = previous_selection(*sel);
                EventResult::Consumed
            }
            (KeyModifiers::NONE, KeyCode::Down) if !ctrl => {
                let mut sel = selected.write();
                *sel = next_selection(*sel, indices.len());
                EventResult::Consumed
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                let mut sel = selected.write();
                *sel = sel.saturating_sub(visible.max(1));
                EventResult::Consumed
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                let mut sel = selected.write();
                let step = visible.max(1);
                *sel = (*sel + step).min(indices.len().saturating_sub(1));
                EventResult::Consumed
            }
            (KeyModifiers::NONE, KeyCode::Tab) => {
                *view.write() = ModelPanelView::Tiers;
                EventResult::Consumed
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let sel = *selected.read();
                apply_selected(&choices, &indices, sel);
                close_active_panel();
                EventResult::Consumed
            }
            // 查询非空时 Esc 只清空查询（符合搜索框直觉）；否则放行给全局 Esc 关面板。
            (KeyModifiers::NONE, KeyCode::Esc) => {
                if query.read().is_empty() {
                    EventResult::Ignored
                } else {
                    query.write().clear();
                    *selected.write() = 0;
                    EventResult::Consumed
                }
            }
            _ => EventResult::Ignored,
        }
    }
}

/// 每帧重算的过滤快照：全量列表 + 命中下标（只持读锁，不克隆整份配置）。
fn filtered_snapshot(query: &str) -> (Vec<usize>, Vec<ModelChoice>) {
    let Some(handle) = crate::kit::atoms::PERI_CONFIG_HANDLE.get() else {
        return (Vec::new(), Vec::new());
    };
    let cfg = handle.read();
    let remote = crate::kit::atoms::MODEL_PANEL_REMOTE.state().read().entries.clone();
    let choices = build_choices(&cfg, &cfg.config.active_alias, &remote);
    let indices = filter_choices(&choices, query);
    (indices, choices)
}

fn list_layout(indices: &[usize], selected: usize, visible: usize) -> ListLayout {
    ListLayout {
        header_rows: 2,
        item_rows: 1,
        footer_rows: 1,
        visible_items: visible as u16,
        scroll_start: crate::kit::list_nav::scroll_start_for_selected(
            selected,
            indices.len(),
            visible,
        ),
        item_count: indices.len(),
    }
}

fn apply_selected(choices: &[ModelChoice], indices: &[usize], selected: usize) {
    let Some(&choice_idx) = indices.get(selected) else {
        return;
    };
    let Some(choice) = choices.get(choice_idx) else {
        return;
    };
    // 选中 env 合成 provider = 先把 env 端点落地成配置 provider（在
    // `edit::apply_model_choice` 内完成），否则 ACP 侧拒绝切换。
    apply_model_choice(&choice.provider_id, &choice.model);
}

#[cfg(test)]
#[path = "list_test.rs"]
mod tests;
