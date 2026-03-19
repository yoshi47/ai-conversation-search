pub mod claude_code;
pub mod codex;
pub mod opencode;

pub use claude_code::ConversationIndexer;

/// Parsed message from a JSONL file.
#[derive(Debug, Clone)]
pub struct Message {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub is_sidechain: bool,
    pub timestamp: Option<String>,
    pub message_type: String,
    pub content: String,
    pub session_id: Option<String>,
    pub is_meta_conversation: bool,
}

/// Metadata extracted from a conversation JSONL file.
#[derive(Debug)]
pub struct ConversationMeta {
    pub summary: Option<String>,
    pub leaf_uuid: Option<String>,
    pub custom_title: Option<String>,
    pub first_user_message: Option<String>,
}
