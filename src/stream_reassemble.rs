use bytes::Bytes;
use futures_util::Stream;
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

const MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024; // 10 MB

pin_project! {
    /// A stream wrapper that forwards all chunks while accumulating them in a buffer.
    /// When the stream ends successfully, sends the accumulated buffer via a oneshot channel.
    pub struct StreamTee<S> {
        #[pin]
        inner: S,
        buffer: Vec<u8>,
        sender: Option<tokio::sync::oneshot::Sender<Vec<u8>>>,
        errored: bool,
    }
}

impl<S> StreamTee<S> {
    pub fn new(inner: S, sender: tokio::sync::oneshot::Sender<Vec<u8>>) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            sender: Some(sender),
            errored: false,
        }
    }
}

impl<S, E> Stream for StreamTee<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match this.inner.poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if !*this.errored && this.buffer.len() + bytes.len() <= MAX_BUFFER_SIZE {
                    this.buffer.extend_from_slice(&bytes);
                } else if !*this.errored {
                    // Buffer exceeded, abandon caching
                    *this.errored = true;
                    this.sender.take(); // drop sender
                    this.buffer.clear();
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                *this.errored = true;
                this.sender.take();
                this.buffer.clear();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                // Stream finished
                if !*this.errored {
                    if let Some(sender) = this.sender.take() {
                        let _ = sender.send(std::mem::take(this.buffer));
                    }
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Reconstruct a non-streaming Anthropic Messages API response from SSE event data.
/// Returns `None` if the stream was incomplete or unparseable.
pub fn reconstruct_response(sse_data: &str) -> Option<Vec<u8>> {
    let mut base_message: Option<serde_json::Value> = None;
    let mut content_blocks: Vec<serde_json::Value> = Vec::new();
    let mut saw_message_stop = false;
    let mut stop_reason: Option<serde_json::Value> = None;
    let mut stop_sequence: Option<serde_json::Value> = None;
    let mut output_usage: Option<serde_json::Value> = None;

    // Track accumulated text/json per content block index
    let mut text_accum: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut json_accum: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut thinking_accum: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();

    for line in sse_data.lines() {
        let line = line.trim();
        if !line.starts_with("data: ") {
            continue;
        }
        let json_str = &line[6..];
        let event: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "message_start" => {
                if let Some(msg) = event.get("message") {
                    base_message = Some(msg.clone());
                }
            }
            "content_block_start" => {
                let index = event
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;
                let block = event
                    .get("content_block")
                    .cloned()
                    .unwrap_or(serde_json::json!({"type": "text", "text": ""}));

                // Ensure content_blocks vec is large enough
                while content_blocks.len() <= index {
                    content_blocks.push(serde_json::json!({"type": "text", "text": ""}));
                }
                content_blocks[index] = block;
            }
            "content_block_delta" => {
                let index = event
                    .get("index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;

                if let Some(delta) = event.get("delta") {
                    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match delta_type {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                text_accum.entry(index).or_default().push_str(text);
                            }
                        }
                        "input_json_delta" => {
                            if let Some(json) =
                                delta.get("partial_json").and_then(|t| t.as_str())
                            {
                                json_accum.entry(index).or_default().push_str(json);
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta.get("thinking").and_then(|t| t.as_str()) {
                                thinking_accum.entry(index).or_default().push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(delta) = event.get("delta") {
                    stop_reason = delta.get("stop_reason").cloned();
                    stop_sequence = delta.get("stop_sequence").cloned();
                }
                if let Some(usage) = event.get("usage") {
                    output_usage = Some(usage.clone());
                }
            }
            "message_stop" => {
                saw_message_stop = true;
            }
            _ => {}
        }
    }

    if !saw_message_stop {
        return None;
    }

    let mut message = base_message?;

    // Assemble final content blocks
    for (index, block) in content_blocks.iter_mut().enumerate() {
        let block_type = block
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        match block_type.as_str() {
            "text" => {
                if let Some(text) = text_accum.get(&index) {
                    block["text"] = serde_json::Value::String(text.clone());
                }
            }
            "tool_use" => {
                if let Some(json_str) = json_accum.get(&index) {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                        block["input"] = parsed;
                    }
                }
            }
            "thinking" => {
                if let Some(text) = thinking_accum.get(&index) {
                    block["thinking"] = serde_json::Value::String(text.clone());
                }
            }
            _ => {}
        }
    }

    message["content"] = serde_json::Value::Array(content_blocks);

    if let Some(sr) = stop_reason {
        message["stop_reason"] = sr;
    }
    if let Some(ss) = stop_sequence {
        message["stop_sequence"] = ss;
    }

    // Merge output usage into existing usage
    if let Some(out_usage) = output_usage {
        if let Some(existing_usage) = message.get_mut("usage") {
            if let Some(obj) = existing_usage.as_object_mut() {
                if let Some(out_obj) = out_usage.as_object() {
                    for (k, v) in out_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }

    // Remove stream-only fields
    if let Some(obj) = message.as_object_mut() {
        obj.remove("delta");
    }

    serde_json::to_vec(&message).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_text_response() {
        let sse = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-20250514\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Ottawa\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" is the capital.\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

        let result = reconstruct_response(sse);
        assert!(result.is_some());

        let json: serde_json::Value = serde_json::from_slice(&result.unwrap()).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"][0]["text"], "Ottawa is the capital.");
        assert_eq!(json["stop_reason"], "end_turn");
        assert_eq!(json["usage"]["output_tokens"], 12);
    }

    #[test]
    fn test_incomplete_stream() {
        let sse = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n";

        let result = reconstruct_response(sse);
        assert!(result.is_none());
    }

    #[test]
    fn test_tool_use_response() {
        let sse = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_02\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"get_weather\",\"input\":{}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\": \"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Ottawa\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":20}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

        let result = reconstruct_response(sse);
        assert!(result.is_some());

        let json: serde_json::Value = serde_json::from_slice(&result.unwrap()).unwrap();
        assert_eq!(json["content"][0]["type"], "tool_use");
        assert_eq!(json["content"][0]["input"]["city"], "Ottawa");
        assert_eq!(json["stop_reason"], "tool_use");
    }
}
