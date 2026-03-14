use crate::metrics::CompressionEvent;
use crate::tokenizer::Tokenizer;

/// Stage C: AST-aware code pruning (stub — implemented in Milestone 8)
pub fn prune(
    _content: &str,
    _user_query: &str,
    _tokenizer: &Tokenizer,
    _min_lines: usize,
) -> (String, Vec<CompressionEvent>) {
    (_content.to_string(), vec![])
}
