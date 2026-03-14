use crate::metrics::CompressionEvent;
use crate::tokenizer::Tokenizer;

/// Stage A: Tool-result deduplication (stub — implemented in Milestone 5)
pub fn dedup(
    _body: &mut serde_json::Value,
    _tokenizer: &Tokenizer,
) -> Vec<CompressionEvent> {
    vec![]
}
