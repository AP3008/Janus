pub mod ast_prune;
pub mod dedup;
pub mod regex_compress;

use std::sync::Arc;

use crate::config::PipelineConfig;
use crate::metrics::{CompressionEvent, ToolCallInfo};
use crate::session::SessionData;
use crate::tokenizer::Tokenizer;

/// Extract the user's latest query text from the messages array
fn extract_user_query(body: &serde_json::Value) -> String {
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        // Find the last user message
        for msg in messages.iter().rev() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    return s.to_string();
                }
                if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in arr {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                return text.to_string();
                            }
                        }
                    }
                }
            }
        }
    }
    String::new()
}

/// Extract all text content from messages for compression (user + tool_result only)
fn extract_compressible_texts(body: &serde_json::Value) -> Vec<(Vec<usize>, String)> {
    let mut texts = Vec::new();

    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for (msg_idx, msg) in messages.iter().enumerate() {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

            // Only compress user messages and tool_result content
            if role == "assistant" {
                continue;
            }

            if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                texts.push((vec![msg_idx], s.to_string()));
            } else if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                for (block_idx, block) in arr.iter().enumerate() {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                texts.push((vec![msg_idx, block_idx], text.to_string()));
                            }
                        }
                        Some("tool_result") => {
                            if let Some(content) = block.get("content").and_then(|c| c.as_str()) {
                                texts.push((vec![msg_idx, block_idx], content.to_string()));
                            } else if let Some(content_arr) =
                                block.get("content").and_then(|c| c.as_array())
                            {
                                for (inner_idx, inner_block) in content_arr.iter().enumerate() {
                                    if let Some(text) =
                                        inner_block.get("text").and_then(|t| t.as_str())
                                    {
                                        texts.push((
                                            vec![msg_idx, block_idx, inner_idx],
                                            text.to_string(),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    texts
}

/// Write compressed text back into the body at the given path
fn write_text_back(body: &mut serde_json::Value, path: &[usize], text: &str) {
    match path.len() {
        1 => {
            // Direct message content (string)
            if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                if let Some(msg) = messages.get_mut(path[0]) {
                    msg["content"] = serde_json::Value::String(text.to_string());
                }
            }
        }
        2 => {
            // Block inside content array
            if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                if let Some(msg) = messages.get_mut(path[0]) {
                    if let Some(arr) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                        if let Some(block) = arr.get_mut(path[1]) {
                            let block_type =
                                block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if block_type == "text" {
                                block["text"] = serde_json::Value::String(text.to_string());
                            } else if block_type == "tool_result" {
                                block["content"] = serde_json::Value::String(text.to_string());
                            }
                        }
                    }
                }
            }
        }
        3 => {
            // Inner block inside tool_result content array
            if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                if let Some(msg) = messages.get_mut(path[0]) {
                    if let Some(arr) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                        if let Some(block) = arr.get_mut(path[1]) {
                            if let Some(inner_arr) =
                                block.get_mut("content").and_then(|c| c.as_array_mut())
                            {
                                if let Some(inner_block) = inner_arr.get_mut(path[2]) {
                                    inner_block["text"] =
                                        serde_json::Value::String(text.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Result of running the pipeline
#[derive(Default)]
pub struct PipelineResult {
    pub events: Vec<CompressionEvent>,
    pub tool_calls: Vec<ToolCallInfo>,
}

/// Run the full compression pipeline on a request body
pub fn process(
    body: &mut serde_json::Value,
    tokenizer: &Tokenizer,
    config: &PipelineConfig,
    session: Option<&Arc<SessionData>>,
) -> PipelineResult {
    let mut all_events = Vec::new();
    let mut all_tool_calls = Vec::new();

    // Stage A: Tool-result deduplication
    if config.tool_dedup {
        if let Some(session) = session {
            let (events, tool_calls) = dedup::dedup(body, session, tokenizer);
            all_events.extend(events);
            all_tool_calls.extend(tool_calls);
        }
    }

    // Extract compressible text segments
    let texts = extract_compressible_texts(body);
    let user_query = extract_user_query(body);

    for (path, text) in texts {
        let mut current_text = text;

        // Stage B: Regex structural compression
        if config.regex_structural {
            let (compressed, events) = regex_compress::compress(&current_text, tokenizer);
            all_events.extend(events);
            current_text = compressed;
        }

        // Stage C: AST pruning
        if config.ast_pruning {
            let (compressed, events) =
                ast_prune::prune(&current_text, &user_query, tokenizer, config.min_lines_for_ast);
            all_events.extend(events);
            current_text = compressed;
        }

        // Write back compressed text
        write_text_back(body, &path, &current_text);
    }

    PipelineResult {
        events: all_events,
        tool_calls: all_tool_calls,
    }
}
