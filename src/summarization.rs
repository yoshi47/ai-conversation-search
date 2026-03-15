use regex::Regex;
use std::sync::LazyLock;

static CMD_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"ai-conversation-search\s+(search|list|index|tree|context|resume)").unwrap(),
        Regex::new(r"ai-conversation-search\s+--\w+\s+(search|list|index|tree|context|resume)").unwrap(),
        Regex::new(r"ai-conversation-search\s+(--help|--version|-h|-v)").unwrap(),
        Regex::new(r"uv\s+tool\s+upgrade\s+ai-conversation-search").unwrap(),
        Regex::new(r"pip\s+install\s+--upgrade\s+ai-conversation-search").unwrap(),
        Regex::new(r"command\s+-v\s+ai-conversation-search").unwrap(),
        Regex::new(r"which\s+ai-conversation-search").unwrap(),
    ]
});

const NOISE_PATTERNS: &[&str] = &[
    "[Tool: Read]",
    "[Tool: Glob]",
    "[Tool: LS]",
    "[Tool: Grep]",
    "[Tool result]",
    "[Request interrupted]",
];

const ACK_PHRASES: &[&str] = &[
    "let me read",
    "let me check",
    "let me search",
    "i'll look at",
    "looking at",
    "checking",
];

/// Detect if a message is pure tool spam that should be filtered.
pub fn is_tool_noise(content: &str, message_type: &str) -> bool {
    let trimmed = content.trim();

    if trimmed == "[Tool result]" {
        return true;
    }
    if trimmed.contains("[Request interrupted") {
        return true;
    }
    if trimmed.is_empty() {
        return true;
    }
    if content.len() < 50 {
        return false; // marked as "too_short" instead
    }

    if NOISE_PATTERNS.iter().any(|p| content.contains(p)) {
        let mut text_without_tools = content.to_string();
        for pattern in NOISE_PATTERNS {
            text_without_tools = text_without_tools.replace(pattern, "");
        }
        let remaining = text_without_tools.trim();
        if remaining.len() > 100 {
            return false;
        }
        return true;
    }

    if message_type == "assistant" && content.len() < 150 {
        let lower = content.to_lowercase();
        if ACK_PHRASES.iter().any(|phrase| lower.contains(phrase)) {
            return true;
        }
    }

    false
}

/// Detect if a message involves using the conversation-search tool.
pub fn message_uses_conversation_search(content: &str, message_type: &str) -> bool {
    if message_type != "assistant" {
        return false;
    }

    // Pattern 1: Bash tool + ai-conversation-search command
    if content.contains("[Tool: Bash]") && content.contains("ai-conversation-search") {
        return true;
    }

    // Pattern 2: Direct command usage
    if content.contains("ai-conversation-search") {
        for re in CMD_PATTERNS.iter() {
            if re.is_match(content) {
                return true;
            }
        }
    }

    let content_lower = content.to_lowercase();

    // Pattern 5: Skill activation markers
    if content_lower.contains("conversation-search skill is loading")
        || content_lower.contains("conversation-search skill is running")
    {
        return true;
    }

    // Pattern 6: Quoted skill name with activation verbs
    if content_lower.contains("\"conversation-search\"") {
        let activation_verbs = ["loading", "running", "is active", "activated"];
        if activation_verbs.iter().any(|v| content_lower.contains(v)) {
            return true;
        }
    }

    // Pattern 7: Skill allowed tools marker
    if content_lower.contains("allowed 1 tools for this command") {
        if let Some(marker_pos) = content_lower.find("allowed 1 tools") {
            if let Some(search_pos) = content_lower.find("conversation-search") {
                let diff = if marker_pos > search_pos {
                    marker_pos - search_pos
                } else {
                    search_pos - marker_pos
                };
                if diff < 100 {
                    return true;
                }
            }
        }
    }

    false
}

/// Detect if this is an automated summarizer conversation.
pub fn is_summarizer_conversation(messages: &[crate::indexer::Message]) -> bool {
    if messages.len() < 2 || messages.len() > 10 {
        return false;
    }

    let first_user = messages.iter().find(|m| m.message_type == "user");
    let first_user = match first_user {
        Some(m) => m,
        None => return false,
    };

    let content = first_user.content.to_lowercase();
    let indicators = [
        "summarize this",
        "create a 1-2 sentence summary",
        "generate concise summaries",
        "max 150 characters",
        "for each message",
        "json output:",
        "brief summary here",
        "messages to summarize:",
    ];

    indicators.iter().any(|ind| content.contains(ind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_noise_tool_result() {
        assert!(is_tool_noise("[Tool result]", "assistant"));
    }

    #[test]
    fn test_tool_noise_empty() {
        assert!(is_tool_noise("", "user"));
        assert!(is_tool_noise("   ", "user"));
    }

    #[test]
    fn test_tool_noise_short_not_noise() {
        assert!(!is_tool_noise("Hello world", "user"));
    }

    #[test]
    fn test_tool_noise_with_substantial_text() {
        let content = format!(
            "[Tool: Read] {} some meaningful content that is more than 100 characters long and should not be filtered as noise because it contains real information",
            "x".repeat(50)
        );
        assert!(!is_tool_noise(&content, "assistant"));
    }

    #[test]
    fn test_message_uses_conversation_search() {
        assert!(message_uses_conversation_search(
            "[Tool: Bash] ai-conversation-search search test",
            "assistant"
        ));
        assert!(!message_uses_conversation_search(
            "ai-conversation-search search test",
            "user"
        ));
    }

    // --- is_tool_noise additional tests ---

    #[test]
    fn test_tool_noise_request_interrupted() {
        assert!(is_tool_noise("[Request interrupted by user]", "user"));
    }

    #[test]
    fn test_tool_noise_with_tools_but_short_remaining() {
        let content = "[Tool: Read] some short text here that is over 50 chars padding padding";
        assert!(is_tool_noise(content, "assistant"));
    }

    #[test]
    fn test_tool_noise_ack_phrase_let_me_check() {
        // Must be >= 50 chars and < 150 chars to hit the ack phrase branch
        let content = "Let me check the file for you, I will look into it right away now";
        assert!(content.len() >= 50 && content.len() < 150);
        assert!(is_tool_noise(content, "assistant"));
    }

    #[test]
    fn test_tool_noise_ack_phrase_wrong_type() {
        let content = "Let me check the file for you, I will look into it right away now";
        assert!(!is_tool_noise(content, "user"));
    }

    #[test]
    fn test_tool_noise_long_ack_not_noise() {
        let content = format!(
            "Let me check the file for you. {}",
            "This is a very detailed explanation that goes on and on. ".repeat(5)
        );
        assert!(content.len() > 150);
        assert!(!is_tool_noise(&content, "assistant"));
    }

    // --- message_uses_conversation_search additional tests ---

    #[test]
    fn test_search_skill_loading_marker() {
        assert!(message_uses_conversation_search(
            "The conversation-search skill is loading now",
            "assistant"
        ));
    }

    #[test]
    fn test_search_skill_running_marker() {
        assert!(message_uses_conversation_search(
            "The conversation-search skill is running",
            "assistant"
        ));
    }

    #[test]
    fn test_search_quoted_skill_with_verb() {
        assert!(message_uses_conversation_search(
            "The \"conversation-search\" plugin has been activated successfully",
            "assistant"
        ));
    }

    #[test]
    fn test_search_allowed_tools_nearby() {
        let content = "conversation-search: allowed 1 tools for this command";
        assert!(message_uses_conversation_search(content, "assistant"));
    }

    #[test]
    fn test_search_allowed_tools_far() {
        let padding = "x".repeat(120);
        let content = format!(
            "conversation-search {}allowed 1 tools for this command",
            padding
        );
        assert!(!message_uses_conversation_search(&content, "assistant"));
    }

    #[test]
    fn test_search_user_message_ignored() {
        assert!(!message_uses_conversation_search(
            "conversation-search skill is loading and allowed 1 tools for this command",
            "user"
        ));
    }

    // --- is_summarizer_conversation tests ---

    use crate::indexer::Message;

    fn make_message(msg_type: &str, content: &str) -> Message {
        Message {
            uuid: "test-uuid".to_string(),
            parent_uuid: None,
            is_sidechain: false,
            timestamp: Some("2025-01-15T10:00:00Z".to_string()),
            message_type: msg_type.to_string(),
            content: content.to_string(),
            session_id: Some("test-session".to_string()),
            is_meta_conversation: false,
        }
    }

    #[test]
    fn test_summarizer_with_summarize_this() {
        let messages = vec![
            make_message("user", "Please summarize this conversation for me"),
            make_message("assistant", "Here is the summary..."),
        ];
        assert!(is_summarizer_conversation(&messages));
    }

    #[test]
    fn test_summarizer_with_json_output() {
        let messages = vec![
            make_message("user", "Process these messages. json output: required"),
            make_message("assistant", "Done."),
        ];
        assert!(is_summarizer_conversation(&messages));
    }

    #[test]
    fn test_summarizer_too_few_messages() {
        let messages = vec![make_message("user", "summarize this")];
        assert!(!is_summarizer_conversation(&messages));
    }

    #[test]
    fn test_summarizer_too_many_messages() {
        let messages: Vec<Message> = (0..11)
            .map(|i| {
                if i == 0 {
                    make_message("user", "summarize this")
                } else if i % 2 == 0 {
                    make_message("user", "more")
                } else {
                    make_message("assistant", "ok")
                }
            })
            .collect();
        assert!(!is_summarizer_conversation(&messages));
    }

    #[test]
    fn test_summarizer_no_user_message() {
        let messages = vec![
            make_message("assistant", "summarize this"),
            make_message("assistant", "ok"),
        ];
        assert!(!is_summarizer_conversation(&messages));
    }

    #[test]
    fn test_summarizer_normal_conversation() {
        let messages = vec![
            make_message("user", "How do I use Rust iterators?"),
            make_message("assistant", "You can use .iter() to create an iterator..."),
            make_message("user", "Thanks!"),
        ];
        assert!(!is_summarizer_conversation(&messages));
    }

    // --- proximity boundary tests ---

    #[test]
    fn test_search_proximity_exact_boundary() {
        // "conversation-search" is 19 chars; we need "allowed 1 tools" to start within 100 chars
        // of "conversation-search". Place padding so the distance is exactly 99.
        let padding = "x".repeat(80); // 19 + 80 = 99 distance
        let content = format!(
            "conversation-search{}allowed 1 tools for this command",
            padding
        );
        assert!(message_uses_conversation_search(&content, "assistant"));
    }

    #[test]
    fn test_search_proximity_over_boundary() {
        // Make distance > 100
        let padding = "x".repeat(101);
        let content = format!(
            "conversation-search{}allowed 1 tools for this command",
            padding
        );
        assert!(!message_uses_conversation_search(&content, "assistant"));
    }
}
