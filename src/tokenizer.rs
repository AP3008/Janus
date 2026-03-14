use tiktoken_rs::CoreBPE;

pub struct Tokenizer {
    bpe: CoreBPE,
}

impl Tokenizer {
    pub fn new() -> Self {
        let bpe = tiktoken_rs::cl100k_base().expect("Failed to load cl100k_base tokenizer");
        Self { bpe }
    }

    /// Count tokens in a text string
    pub fn count_tokens(&self, text: &str) -> usize {
        self.bpe.encode_ordinary(text).len()
    }

    /// Count total tokens across all message content in an Anthropic messages body
    pub fn count_message_tokens(&self, body: &serde_json::Value) -> usize {
        let mut total = 0;

        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            for msg in messages {
                total += self.count_content_tokens(msg.get("content"));
            }
        }

        // Count system prompt tokens if present
        if let Some(system) = body.get("system") {
            if let Some(s) = system.as_str() {
                total += self.count_tokens(s);
            } else if let Some(arr) = system.as_array() {
                for block in arr {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        total += self.count_tokens(text);
                    }
                }
            }
        }

        // Count tool definitions tokens if present
        if let Some(tools) = body.get("tools") {
            total += self.count_tokens(&tools.to_string());
        }

        total
    }

    /// Count tokens in a content field (can be string or array of blocks)
    fn count_content_tokens(&self, content: Option<&serde_json::Value>) -> usize {
        let Some(content) = content else {
            return 0;
        };

        if let Some(s) = content.as_str() {
            return self.count_tokens(s);
        }

        if let Some(arr) = content.as_array() {
            let mut total = 0;
            for block in arr {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            total += self.count_tokens(text);
                        }
                    }
                    Some("tool_result") => {
                        total += self.count_content_tokens(block.get("content"));
                    }
                    Some("tool_use") => {
                        if let Some(input) = block.get("input") {
                            total += self.count_tokens(&input.to_string());
                        }
                    }
                    _ => {
                        // For other block types, count the JSON representation
                        total += self.count_tokens(&block.to_string());
                    }
                }
            }
            return total;
        }

        self.count_tokens(&content.to_string())
    }
}
