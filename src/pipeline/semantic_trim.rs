use crate::metrics::CompressionEvent;
use crate::tokenizer::Tokenizer;

/// Stage D: Semantic trim (stub — out of scope for v1)
pub fn trim(
    _content: &str,
    _query_embedding: Option<&[f32]>,
    _threshold: f64,
    _tokenizer: &Tokenizer,
) -> (String, Vec<CompressionEvent>) {
    (_content.to_string(), vec![])
}
