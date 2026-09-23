//! `model_speed` 纯逻辑测试：估算口径、状态机、快照计算。
//!
//! 全局 `TRACKER` 是进程级共享状态，用例必须串行（`#[serial]`）并在开头 `reset()`。

use super::*;
use serial_test::serial;

#[test]
fn estimate_counts_cjk_as_one_and_ascii_as_quarter() {
    // 4 个 ASCII 字符 = 1 token
    assert!((estimate_tokens("abcd") - 1.0).abs() < f64::EPSILON);
    // 4 个 CJK 字符 = 4 token
    assert!((estimate_tokens("中文测试") - 4.0).abs() < f64::EPSILON);
    // 混合：2 ASCII(0.5) + 2 CJK(2) = 2.5
    assert!((estimate_tokens("ab中文") - 2.5).abs() < f64::EPSILON);
    // 空串
    assert!((estimate_tokens("") - 0.0).abs() < f64::EPSILON);
}

#[test]
#[serial]
fn snapshot_is_none_before_any_activity() {
    reset();
    assert!(snapshot().is_none(), "无活动时不应有速率段");
}

#[test]
#[serial]
fn chunks_start_the_clock_and_accumulate_tokens() {
    reset();
    note_prompt_submitted();
    note_chunk("abcd"); // 1 token
    note_chunk("efgh"); // 1 token
    let snap = snapshot().expect("生成中应有快照");
    assert!(snap.live, "收到 chunk 后应处于生成中");
    assert!(snap.approx, "生成中的速率必然是按字符估算的");
    assert_eq!(snap.output_tokens, 2);
    assert!(snap.tps >= 0.0);
}

#[test]
#[serial]
fn empty_chunk_does_not_start_the_clock() {
    reset();
    note_prompt_submitted();
    note_chunk("");
    assert!(snapshot().is_none(), "空 chunk 不应把状态推进到生成中");
}

#[test]
#[serial]
fn real_output_tokens_replace_the_estimate_after_turn_end() {
    reset();
    note_prompt_submitted();
    note_chunk("abcd"); // 估算 1 token
    note_output_tokens(500); // 真实 500
    note_turn_ended();
    let snap = snapshot().expect("结束后仍应保留上一轮读数");
    assert!(!snap.live);
    assert!(!snap.approx, "拿到真实 token 数后不再是估算");
    assert_eq!(snap.output_tokens, 500);
}

#[test]
#[serial]
fn estimate_is_kept_when_no_real_tokens_arrived() {
    reset();
    note_prompt_submitted();
    note_chunk("abcdefgh"); // 估算 2 token
    note_turn_ended();
    let snap = snapshot().expect("结束后仍应保留上一轮读数");
    assert!(!snap.live);
    assert!(snap.approx, "没拿到真实 token 数时仍标记为估算");
    assert_eq!(snap.output_tokens, 2);
}

#[test]
#[serial]
fn turn_end_without_chunks_keeps_no_reading() {
    reset();
    note_prompt_submitted();
    note_turn_ended();
    assert!(snapshot().is_none(), "没有首个 token 就没有速率可算");
}

#[test]
#[serial]
fn prompt_submitted_resets_the_previous_round() {
    reset();
    note_prompt_submitted();
    note_chunk("abcd");
    note_turn_ended();
    assert!(snapshot().is_some());

    // 下一轮开始：上一轮的读数必须被清掉，否则状态栏会短暂显示旧值
    note_prompt_submitted();
    assert!(snapshot().is_none(), "新一轮开始应清空上一轮读数");
}

#[test]
#[serial]
fn reset_clears_everything() {
    reset();
    note_prompt_submitted();
    note_chunk("abcd");
    reset();
    assert!(snapshot().is_none());
}
