//! 身份切换提示的纯文本辅助函数；调用方须先确认错误属于会话轮数上限。

use serde_json::Value;
use std::sync::OnceLock;

/// 只从既有错误信封的 message 提取已知轮数提示，不扫描其他字段或猜测数字。
/// 与 is_turn_limit 的对象选择一致：优先根 error，否则使用根对象；不递归搜索 data。
pub(super) fn turn_count(error_body: &str) -> Option<u64> {
    let parsed: Value = serde_json::from_str(error_body).ok()?;
    let error = parsed.get("error").unwrap_or(&parsed);
    let message = error.get("message")?.as_str()?;
    static KNOWN_COUNT: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = KNOWN_COUNT.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)(?:已回复\s*([1-9][0-9]*)\s*轮",
            r"|\bhas reached(?: the limit of)?\s+([1-9][0-9]*)\s+model turns\b)"
        ))
        .expect("固定轮数提示正则合法")
    });
    let mut count = None;
    for captures in pattern.captures_iter(message) {
        let value = captures.get(1).or_else(|| captures.get(2))?;
        let value = value.as_str().parse::<u64>().ok()?;
        if count.is_some_and(|previous| previous != value) {
            return None;
        }
        count = Some(value);
    }
    count
}

pub(super) fn notice_text(turn_count: Option<u64>) -> String {
    match turn_count {
        Some(count) => format!("身份已切换（第{count}轮回复），聊天记录已保留。"),
        None => "身份已切换，聊天记录已保留。".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error(message: &str) -> String {
        json!({"error":{"code":"session_turn_limit_exceeded","message":message}}).to_string()
    }

    #[test]
    fn chinese_and_both_known_english_forms_supply_the_exact_count() {
        for (message, expected) in [
            ("当前会话模型已回复 100 轮，达到上限。", 100),
            ("当前会话模型已回复105轮，达到上限。", 105),
            ("This conversation has reached 107 model turns.", 107),
            (
                "This session has reached the limit of 109 model turns.",
                109,
            ),
            ("THIS CONVERSATION HAS REACHED 111 MODEL TURNS.", 111),
        ] {
            assert_eq!(turn_count(&error(message)), Some(expected));
        }
    }

    #[test]
    fn matching_bilingual_and_repeated_counts_are_unambiguous() {
        assert_eq!(
            turn_count(&error("当前会话模型已回复 100 轮，达到上限。This conversation has reached 100 model turns. 已回复 100 轮")),
            Some(100)
        );
    }

    #[test]
    fn conflicting_counts_are_never_guessed() {
        for message in [
            "当前会话模型已回复 100 轮。This conversation has reached 105 model turns.",
            "当前会话模型已回复 100 轮，旧记录已回复 99 轮。",
        ] {
            assert_eq!(turn_count(&error(message)), None);
        }
    }

    #[test]
    fn numeric_fields_and_quoted_noise_do_not_replace_the_known_count() {
        let body = json!({
            "status":400,
            "message":"已回复 12 轮",
            "data":{"message":"has reached 13 model turns"},
            "error":{
                "code":"session_turn_limit_exceeded",
                "message":"HTTP 400: \"request 999\"; 当前会话模型已回复 105 轮，达到上限。",
                "turns":200
            }
        });
        assert_eq!(turn_count(&body.to_string()), Some(105));
        assert_eq!(
            turn_count(&error("HTTP 400: \"100\"，request_id=105")),
            None
        );
        assert_eq!(turn_count(&error("已回复 \"105\" 轮")), None);
    }

    #[test]
    fn absent_malformed_or_unrelated_messages_have_no_count() {
        for body in [
            "not JSON",
            "当前会话模型已回复 100 轮，达到上限。",
            r#"{"error":{"message":"达到上限，请继续。"}}"#,
            r#"{"error":{"message":100}}"#,
            r#"{"error":"已回复 100 轮"}"#,
            r#"{"message":"已回复 100 轮","error":{}}"#,
            r#"{"data":{"error":{"message":"已回复 100 轮"}}}"#,
            r#"{"input":[{"message":"has reached 105 model turns"}]}"#,
        ] {
            assert_eq!(turn_count(body), None);
        }
    }

    #[test]
    fn normalized_root_error_object_is_supported() {
        assert_eq!(
            turn_count(r#"{"code":"session_turn_limit_exceeded","message":"已回复 100 轮"}"#),
            Some(100)
        );
    }

    #[test]
    fn overflow_negative_fractional_and_incomplete_counts_are_not_inferred() {
        for message in [
            "已回复 18446744073709551616 轮",
            "已回复 -100 轮",
            "已回复 100.5 轮",
            "has reached 105.5 model turns",
            "has reached model turns",
            "105 model turns",
            "已回复 0 轮",
        ] {
            assert_eq!(turn_count(&error(message)), None);
        }
        assert_eq!(
            turn_count(&error("已回复 18446744073709551615 轮")),
            Some(u64::MAX)
        );
    }

    #[test]
    fn notices_use_exact_chinese_text_without_an_invented_count() {
        assert_eq!(
            notice_text(Some(105)),
            "身份已切换（第105轮回复），聊天记录已保留。"
        );
        assert_eq!(notice_text(None), "身份已切换，聊天记录已保留。");
    }
}
