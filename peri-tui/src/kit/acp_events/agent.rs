//! Agent event handlers — AgentExecutionFailed, BackgroundTaskCompleted.

use super::*;
use crate::i18n;
use crate::kit::tui_render_unit::TuiNoteLevel;
use fluent_bundle::FluentValue;

pub(super) fn handle_agent_execution_failed(state: &mut BridgeState, message: &str) {
    tracing::error!("bridge: AgentExecutionFailed");
    state.pending_cache_usage = None;
    state.phase = SessionPhase::Idle;
    state.current_turn.deactivate();
    // LLM/HTTP 类失败时把当前模型名带进文案。
    //
    // peri 的用户可见错误是**故意脱敏**的（`peri-acp-types/src/error.rs` 的
    // `user_facing_message` + `SafeModelErrorDiagnostic` 只带 status/request_id），
    // 光看 “An LLM API error occurred (HTTP 502)” 无法判断是哪个模型挂了。
    // 而网关的 `/models` 常列出它上游服务不了的模型，用户选中就 502 ——
    // 不带模型名基本无法定位（只能翻 ~/.peri/logs）。
    //
    // 模型名不是秘密：状态栏和 /model 面板本来就显示它，所以这里补上不破坏脱敏策略
    // （脱敏针对的是凭据、端点、请求体）。
    let model = crate::kit::atoms::SERVICE_SNAPSHOT
        .state()
        .read()
        .model_name
        .clone();
    let looks_like_llm_error = message.contains("LLM") || message.contains("HTTP");
    let text = if looks_like_llm_error && !model.is_empty() {
        i18n::tr_args(
            "app-note-agent-failed-model",
            &[
                ("model".into(), FluentValue::from(model.as_str())),
                ("message".into(), FluentValue::from(message)),
            ],
        )
    } else {
        i18n::tr_args(
            "app-note-agent-failed",
            &[("message".into(), FluentValue::from(message))],
        )
    };
    let content_hash = crate::kit::tui_render_unit::tui_hash_str(&text);
    state
        .current_turn
        .push_system_note(text, TuiNoteLevel::Error, content_hash);
    super::render::push_view_models(state);
    super::render::push_acp_state(state);
}

pub(super) fn handle_background_task_completed(
    agent_name: &str,
    task_id: &str,
    success: bool,
    duration_ms: u64,
) {
    let msg = if success {
        format!(
            "后台 {} {} 完成 ({:.0}s)",
            agent_name,
            task_id,
            duration_ms as f64 / 1000.0
        )
    } else {
        format!(
            "后台 {} {} 失败 ({:.0}s)",
            agent_name,
            task_id,
            duration_ms as f64 / 1000.0
        )
    };
    tracing::info!(msg, "bridge: BackgroundTaskCompleted");
}
