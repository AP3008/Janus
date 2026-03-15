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
    let mut signature_accum: std::collections::HashMap<usize, String> =
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
                        "signature_delta" => {
                            if let Some(sig) = delta.get("signature").and_then(|t| t.as_str()) {
                                signature_accum.entry(index).or_default().push_str(sig);
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
                let text = thinking_accum.get(&index).cloned().unwrap_or_default();
                block["thinking"] = serde_json::Value::String(text);
                if let Some(sig) = signature_accum.get(&index) {
                    block["signature"] = serde_json::Value::String(sig.clone());
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

/// Convert a non-streaming Messages API JSON response into SSE format.
/// Used when returning a cached response to a client that requested `stream: true`.
pub fn json_to_sse(response_json: &[u8]) -> Option<Vec<u8>> {
    let msg: serde_json::Value = serde_json::from_slice(response_json).ok()?;
    let mut out = Vec::new();

    // Build message_start: the message object with empty content and no stop_reason yet
    let mut start_msg = msg.clone();
    start_msg["content"] = serde_json::json!([]);
    start_msg["stop_reason"] = serde_json::Value::Null;
    start_msg["stop_sequence"] = serde_json::Value::Null;
    // Strip output_tokens from usage in message_start (it starts at 0)
    if let Some(usage) = start_msg.get_mut("usage") {
        if let Some(obj) = usage.as_object_mut() {
            obj.remove("output_tokens");
        }
    }

    write_sse_event(
        &mut out,
        "message_start",
        &serde_json::json!({"type": "message_start", "message": start_msg}),
    );

    // Emit content blocks
    let content = msg.get("content").and_then(|c| c.as_array());
    if let Some(blocks) = content {
        for (index, block) in blocks.iter().enumerate() {
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("text");

            // content_block_start
            let start_block = match block_type {
                "text" => serde_json::json!({"type": "text", "text": ""}),
                "tool_use" => {
                    serde_json::json!({
                        "type": "tool_use",
                        "id": block.get("id").cloned().unwrap_or(serde_json::json!("")),
                        "name": block.get("name").cloned().unwrap_or(serde_json::json!("")),
                        "input": {}
                    })
                }
                "thinking" => serde_json::json!({"type": "thinking", "thinking": ""}),
                _ => block.clone(),
            };
            write_sse_event(
                &mut out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": start_block
                }),
            );

            // content_block_delta
            match block_type {
                "text" => {
                    let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        write_sse_event(
                            &mut out,
                            "content_block_delta",
                            &serde_json::json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {"type": "text_delta", "text": text}
                            }),
                        );
                    }
                }
                "tool_use" => {
                    if let Some(input) = block.get("input") {
                        let json_str = serde_json::to_string(input).unwrap_or_default();
                        if !json_str.is_empty() {
                            write_sse_event(
                                &mut out,
                                "content_block_delta",
                                &serde_json::json!({
                                    "type": "content_block_delta",
                                    "index": index,
                                    "delta": {"type": "input_json_delta", "partial_json": json_str}
                                }),
                            );
                        }
                    }
                }
                "thinking" => {
                    let thinking = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                    if !thinking.is_empty() {
                        write_sse_event(
                            &mut out,
                            "content_block_delta",
                            &serde_json::json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {"type": "thinking_delta", "thinking": thinking}
                            }),
                        );
                    }
                    if let Some(sig) = block.get("signature").and_then(|s| s.as_str()) {
                        if !sig.is_empty() {
                            write_sse_event(
                                &mut out,
                                "content_block_delta",
                                &serde_json::json!({
                                    "type": "content_block_delta",
                                    "index": index,
                                    "delta": {"type": "signature_delta", "signature": sig}
                                }),
                            );
                        }
                    }
                }
                _ => {}
            }

            // content_block_stop
            write_sse_event(
                &mut out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": index}),
            );
        }
    }

    // message_delta
    let mut delta = serde_json::Map::new();
    delta.insert("type".to_string(), serde_json::json!("message_delta"));

    let mut delta_inner = serde_json::Map::new();
    delta_inner.insert(
        "stop_reason".to_string(),
        msg.get("stop_reason")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    delta_inner.insert(
        "stop_sequence".to_string(),
        msg.get("stop_sequence")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    delta.insert("delta".to_string(), serde_json::Value::Object(delta_inner));

    // Include output_tokens in usage
    if let Some(usage) = msg.get("usage") {
        if let Some(output_tokens) = usage.get("output_tokens") {
            delta.insert(
                "usage".to_string(),
                serde_json::json!({"output_tokens": output_tokens}),
            );
        }
    }

    write_sse_event(
        &mut out,
        "message_delta",
        &serde_json::Value::Object(delta),
    );

    // message_stop
    write_sse_event(
        &mut out,
        "message_stop",
        &serde_json::json!({"type": "message_stop"}),
    );

    Some(out)
}

fn write_sse_event(out: &mut Vec<u8>, event_type: &str, data: &serde_json::Value) {
    use std::fmt::Write;
    let json = serde_json::to_string(data).unwrap_or_default();
    let mut s = String::new();
    let _ = write!(s, "event: {}\ndata: {}\n\n", event_type, json);
    out.extend_from_slice(s.as_bytes());
}
