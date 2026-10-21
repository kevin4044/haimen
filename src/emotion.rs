//! 助手回复开头的情绪 emoji → 小智 emotion 名称。
//!
//! Hermes 侧已经约定：每条回复开头带一个情绪 emoji。这里把它映射成
//! 固件/主题认识的 21 个 emotion key，并从 TTS 文本里剥掉，避免把 emoji
//! 念出来。映射与官方服务端 `textUtils.EMOJI_MAP` 以及 assets 主题的
//! `emoji_collection` 名称保持一致。

/// 一个情绪信号：emotion 名称 + 原始 emoji。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmotionSignal {
    pub emotion: &'static str,
    pub emoji: &'static str,
}

/// 21 个标准情绪：`(emoji, emotion)`。
///
/// 顺序与 assets 主题的 21 个槽位一致；`neutral` 也在其中。
pub const EMOJI_MAP: [(&str, &str); 21] = [
    ("😶", "neutral"),
    ("🙂", "happy"),
    ("😆", "laughing"),
    ("😂", "funny"),
    ("😔", "sad"),
    ("😠", "angry"),
    ("😭", "crying"),
    ("😍", "loving"),
    ("😳", "embarrassed"),
    ("😲", "surprised"),
    ("😱", "shocked"),
    ("🤔", "thinking"),
    ("😉", "winking"),
    ("😎", "cool"),
    ("😌", "relaxed"),
    ("🤤", "delicious"),
    ("😘", "kissy"),
    ("😏", "confident"),
    ("😴", "sleepy"),
    ("😜", "silly"),
    ("🙄", "confused"),
];

/// 常见 emoji 变体/别名 → 21 个标准情绪名。
///
/// Hermes 只要“开头带一个表情”，不保证正好是官方那 21 个字符；这里把
/// 常见的近义 emoji 归一到标准名，避免模型换一个笑脸就没有表情。
const EMOJI_ALIASES: [(&str, &str); 27] = [
    ("😐", "neutral"),
    ("😑", "neutral"),
    ("😊", "happy"),
    ("😀", "happy"),
    ("😃", "happy"),
    ("😄", "happy"),
    ("☺", "happy"),
    ("🤣", "funny"),
    ("😞", "sad"),
    ("😢", "crying"),
    ("😡", "angry"),
    ("🤬", "angry"),
    ("🥰", "loving"),
    ("❤", "loving"),
    ("😯", "surprised"),
    ("😮", "surprised"),
    ("🤯", "shocked"),
    ("💭", "thinking"),
    ("😗", "kissy"),
    ("🥱", "sleepy"),
    ("😝", "silly"),
    ("😕", "confused"),
    ("😟", "sad"),
    ("😤", "angry"),
    ("😸", "happy"),
    ("🥲", "happy"),
    ("😹", "laughing"),
];

fn lookup(first: &str) -> Option<(&'static str, &'static str)> {
    EMOJI_MAP
        .iter()
        .chain(EMOJI_ALIASES.iter())
        .find(|(emoji, _)| *emoji == first)
        .copied()
}

/// 提取回复**开头**的情绪 emoji，并返回去掉 emoji 后的文本。
///
/// 只认第一个非空白字符；不是已知情绪 emoji 时不改动文本，也不产生信号。
/// 支持 emoji 后跟 variation selector（U+FE0F）。
pub fn extract_leading_emotion(text: &str) -> (Option<EmotionSignal>, String) {
    let trimmed = text.trim_start();
    let Some(first) = trimmed.chars().next() else {
        return (None, text.to_owned());
    };
    let first_str = first.to_string();
    let Some((emoji, emotion)) = lookup(&first_str) else {
        return (None, text.to_owned());
    };

    let mut rest = &trimmed[first.len_utf8()..];
    if let Some(stripped) = rest.strip_prefix('\u{fe0f}') {
        rest = stripped;
    }
    let cleaned = rest.trim_start().to_owned();
    (Some(EmotionSignal { emotion, emoji }), cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_21_emotions() {
        for (emoji, emotion) in EMOJI_MAP {
            let text = format!("{emoji}你好");
            let (signal, cleaned) = extract_leading_emotion(&text);
            let signal = signal.expect("应识别");
            assert_eq!(signal.emoji, emoji);
            assert_eq!(signal.emotion, emotion);
            assert_eq!(cleaned, "你好");
        }
    }

    #[test]
    fn strips_leading_space_and_variation_selector() {
        let (signal, cleaned) = extract_leading_emotion("  🙂\u{fe0f}  你好");
        assert_eq!(signal.expect("应识别").emotion, "happy");
        assert_eq!(cleaned, "你好");
    }

    #[test]
    fn unknown_or_no_emoji_is_unchanged() {
        for text in ["你好", "🦄你好", " hello"] {
            let (signal, cleaned) = extract_leading_emotion(text);
            assert!(signal.is_none());
            assert_eq!(cleaned, text);
        }
    }

    #[test]
    fn only_emoji_yields_empty_text() {
        let (signal, cleaned) = extract_leading_emotion("😔");
        assert_eq!(signal.expect("应识别").emotion, "sad");
        assert!(cleaned.is_empty());
    }
}
