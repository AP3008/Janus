use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

use crate::metrics::{CompressionEvent, ToolCallInfo, ToolCallStatus};
use crate::session::{SessionData, ToolResultEntry};
use crate::tokenizer::Tokenizer;

/// A pending dedup action to apply after analysis
struct DedupAction {
    msg_idx: usize,
    block_idx: usize,
    replacement: String,
    event: CompressionEvent,
    tool_call: ToolCallInfo,
}

/// Stage A: Tool-result deduplication
pub fn dedup(
    body: &mut serde_json::Value,
    session: &Arc<SessionData>,
    tokenizer: &Tokenizer,
) -> (Vec<CompressionEvent>, Vec<ToolCallInfo>) {
    let mut tool_calls = Vec::new();
    let mut actions: Vec<DedupAction> = Vec::new();

    // First pass: read-only analysis
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        // Build tool_use_id -> tool_name map from assistant messages
        let mut tool_name_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for msg in messages {
            if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            if let (Some(id), Some(name)) = (
                                block.get("id").and_then(|i| i.as_str()),
                                block.get("name").and_then(|n| n.as_str()),
                            ) {
                                tool_name_map.insert(id.to_string(), name.to_string());
                            }
                        }
                    }
                }
            }
        }

        for (msg_idx, msg) in messages.iter().enumerate() {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "user" {
                continue;
            }

            let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) else {
                continue;
            };

            for (block_idx, block) in content_arr.iter().enumerate() {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }

                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();

                let content_str =
                    if let Some(s) = block.get("content").and_then(|c| c.as_str()) {
                        s.to_string()
                    } else if let Some(content_val) = block.get("content") {
                        content_val.to_string()
                    } else {
                        continue;
                    };

                let hash = xxh3_64(content_str.as_bytes());
                let token_count = tokenizer.count_tokens(&content_str);
                let tool_name = tool_name_map
                    .get(&tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());

                if let Some(existing) = session.tool_hashes.get(&hash) {
                    let replacement = format!(
                        "[Janus: content already provided in tool_use_id {} — {} tokens omitted. Re-reference if needed.]",
                        existing.tool_use_id, existing.original_token_count
                    );
                    let tokens_after = tokenizer.count_tokens(&replacement);

                    actions.push(DedupAction {
                        msg_idx,
                        block_idx,
                        replacement,
                        event: CompressionEvent {
                            tokens_before: token_count,
                            tokens_after,
                            stage_name: "A_dedup".to_string(),
                            reason: format!("duplicate tool_result {}", tool_use_id),
                            timestamp: std::time::Instant::now(),
                        },
                        tool_call: ToolCallInfo {
                            tool_name,
                            input_summary: truncate_str(&content_str, 40),
                            tool_use_id: tool_use_id.clone(),
                            status: ToolCallStatus::Deduped,
                            tokens_saved: token_count.saturating_sub(tokens_after),
                        },
                    });
                } else {
                    session.tool_hashes.insert(
                        hash,
                        ToolResultEntry {
                            tool_use_id: tool_use_id.clone(),
                            original_token_count: token_count,
                        },
                    );

                    tool_calls.push(ToolCallInfo {
                        tool_name,
                        input_summary: truncate_str(&content_str, 40),
                        tool_use_id: tool_use_id.clone(),
                        status: ToolCallStatus::Kept,
                        tokens_saved: 0,
                    });
                }
            }
        }
    }

    // Second pass: apply mutations
    let mut events = Vec::new();
    for action in actions {
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            if let Some(msg) = messages.get_mut(action.msg_idx) {
                if let Some(content_arr) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                    if let Some(block) = content_arr.get_mut(action.block_idx) {
                        block["content"] = serde_json::Value::String(action.replacement);
                    }
                }
            }
        }
        events.push(action.event);
        tool_calls.push(action.tool_call);
    }

    (events, tool_calls)
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}
