//! 模型输出速率（tok/s）统计。
//!
//! ## 为什么要按字符估算
//!
//! `usage_update` 只在 **LLM 调用结束时**发一次（`peri-acp/src/event/mapper.rs`
//! 的 `LlmCallEnd` 分支），生成过程中没有可用的 token 计数。要做**实时**读数
//! 只能按已收到的文本估算。
//!
//! 估算口径与 pi 的 `model-speed` 扩展一致：CJK / emoji ≈ 1 token/字符，
//! 其余 ≈ 0.25 token/字符（即约 4 字符 1 token）。生成结束后若拿到了真实的
//! `output_tokens`，用它覆盖估算值，此时 `approx = false`。
//!
//! ## 线程模型
//!
//! 写入发生在 ACP 事件泵（`acp_events/streaming.rs` 每个 chunk），读取发生在
//! `service_snapshot` 的 2 秒 tick。用 `parking_lot::Mutex` 保护，**不进 atom**——
//! 进 atom 会让状态栏每个 chunk 重渲染一次，长会话下就是那个 CPU 自旋问题。

use std::time::Instant;

use parking_lot::Mutex;

/// 一轮生成的速率状态。
#[derive(Debug, Clone)]
pub struct SpeedTracker {
    /// 本轮是否正在生成。
    active: bool,
    /// 本轮首个 token 到达时刻（TTFT 与速率的分母基准）。
    first_token_at: Option<Instant>,
    /// 本轮按字符估算的 token 数。
    est_tokens: f64,
    /// 上一轮的真实 output_tokens（来自 usage_update；None 表示未拿到）。
    last_output_tokens: Option<u64>,
    /// 上一轮 首个 token → 结束 的秒数。
    ///
    /// 用 f64 秒而不是整数毫秒：极快的响应（或测试）里 `as_millis()` 会
    /// 四舍五入到 0，让速率算不出来。
    last_generation_secs: Option<f64>,
    /// 上一轮 TTFT（prompt 提交 → 首个 token）。
    last_ttft_ms: Option<u64>,
    /// prompt 提交时刻（TTFT 起点）。
    prompt_at: Option<Instant>,
}

impl Default for SpeedTracker {
    fn default() -> Self {
        Self {
            active: false,
            first_token_at: None,
            est_tokens: 0.0,
            last_output_tokens: None,
            last_generation_secs: None,
            last_ttft_ms: None,
            prompt_at: None,
        }
    }
}

/// 给状态栏读的一帧快照。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSpeed {
    /// token / 秒。
    pub tps: f64,
    /// 是否为估算值（生成中，或结束时没拿到真实 token 数）。
    pub approx: bool,
    /// 已生成 token 数（估算或真实）。
    pub output_tokens: u64,
    /// 是否仍在生成。
    pub live: bool,
}

static TRACKER: Mutex<Option<SpeedTracker>> = Mutex::new(None);

/// 粗估一段文本的 token 数（CJK/emoji ≈ 1，其余 ≈ 0.25）。
fn estimate_tokens(text: &str) -> f64 {
    let mut tokens = 0.0f64;
    for ch in text.chars() {
        tokens += if (ch as u32) > 0x2e80 { 1.0 } else { 0.25 };
    }
    tokens
}

/// prompt 提交：重置本轮状态并记下 TTFT 起点。
///
/// 同时清掉上一轮的冻结读数——否则新一轮开始到首个 token 到达之间，
/// 状态栏会短暂显示上一轮的 tok/s。
pub fn note_prompt_submitted() {
    let mut guard = TRACKER.lock();
    let now = Instant::now();
    match guard.as_mut() {
        Some(t) => {
            t.active = false;
            t.first_token_at = None;
            t.est_tokens = 0.0;
            t.prompt_at = Some(now);
            t.last_output_tokens = None;
            t.last_generation_secs = None;
            t.last_ttft_ms = None;
        }
        None => {
            *guard = Some(SpeedTracker {
                prompt_at: Some(now),
                ..Default::default()
            })
        }
    }
}

/// 收到一个流式 chunk：首次调用时开始计时，之后累加估算 token。
///
/// 推理（thinking）与正文都计入——两者都以同一速率生成，只算正文会让
/// thinking 阶段读数一直是 0。
pub fn note_chunk(text: &str) {
    if text.is_empty() {
        return;
    }
    let mut guard = TRACKER.lock();
    let tracker = guard.get_or_insert_with(SpeedTracker::default);
    if !tracker.active {
        tracker.active = true;
        tracker.first_token_at = Some(Instant::now());
    }
    tracker.est_tokens += estimate_tokens(text);
}

/// 收到 `usage_update` 的真实 output_tokens。
pub fn note_output_tokens(output_tokens: u64) {
    let mut guard = TRACKER.lock();
    let tracker = guard.get_or_insert_with(SpeedTracker::default);
    tracker.last_output_tokens = Some(output_tokens);
}

/// 本轮结束（完成 / 打断 / 挂起）：冻结统计。
pub fn note_turn_ended() {
    let mut guard = TRACKER.lock();
    let Some(tracker) = guard.as_mut() else {
        return;
    };
    if !tracker.active {
        return;
    }
    let now = Instant::now();
    if let Some(first) = tracker.first_token_at {
        tracker.last_generation_secs = Some(now.duration_since(first).as_secs_f64());
        if let Some(prompt) = tracker.prompt_at {
            tracker.last_ttft_ms = Some(first.duration_since(prompt).as_millis() as u64);
        }
    }
    tracker.active = false;
}

/// 清空（新建会话 / 切换会话时调用）。
pub fn reset() {
    *TRACKER.lock() = None;
}

/// 给状态栏取一帧。没有数据时返回 `None`（状态栏就不显示这一段）。
pub fn snapshot() -> Option<ModelSpeed> {
    let guard = TRACKER.lock();
    let tracker = guard.as_ref()?;

    if tracker.active {
        // 生成中：估算速率 = 已生成 token / 首个 token 至今。
        // elapsed 为 0（首个 token 刚到）时记 0 而不是返回 None——
        // 返回 None 会让状态栏在响应很快时整段不显示。
        let elapsed_secs = tracker
            .first_token_at
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let tps = if elapsed_secs > 0.0 {
            tracker.est_tokens / elapsed_secs
        } else {
            0.0
        };
        return Some(ModelSpeed {
            tps,
            approx: true,
            output_tokens: tracker.est_tokens as u64,
            live: true,
        });
    }

    // 已结束：优先用真实 output_tokens，缺失时退回估算值。
    // 没跑过一轮（没有 last_generation_secs）才返回 None。
    let gen_secs = tracker.last_generation_secs?;
    let (tokens, approx) = match tracker.last_output_tokens {
        Some(real) => (real as f64, false),
        None => (tracker.est_tokens, true),
    };
    Some(ModelSpeed {
        tps: if gen_secs > 0.0 {
            tokens / gen_secs
        } else {
            0.0
        },
        approx,
        output_tokens: tokens as u64,
        live: false,
    })
}

#[cfg(test)]
#[path = "model_speed_test.rs"]
mod tests;
