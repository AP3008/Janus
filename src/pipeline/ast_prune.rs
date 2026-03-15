use crate::metrics::CompressionEvent;
use crate::tokenizer::Tokenizer;
use regex::Regex;
use lazy_static::lazy_static;

lazy_static! {
    /// Match fenced code blocks with language tag
    static ref FENCED_BLOCK_RE: Regex = Regex::new(
        r"(?ms)^```(\w+)\n(.*?)^```"
    ).unwrap();
}

/// Supported languages and their canonical names
fn detect_language(tag: &str) -> Option<&'static str> {
    match tag.to_lowercase().as_str() {
        "python" | "py" => Some("python"),
        "javascript" | "js" => Some("javascript"),
        "typescript" | "ts" | "tsx" | "jsx" => Some("javascript"), // TS parsed as JS
        "rust" | "rs" => Some("rust"),
        "go" | "golang" => Some("go"),
        _ => None,
    }
}

/// A function/method/class found in parsed code
struct FunctionInfo {
    name: String,
    signature: String,
    start_byte: usize,
    end_byte: usize,
    body_token_count: usize,
    is_referenced: bool,
}

/// Stage C: AST-aware code pruning
///
/// For fenced code blocks of 30+ lines with a detectable language tag,
/// parse with tree-sitter, find top-level functions/classes, and prune
/// bodies of unreferenced functions (those not mentioned in user query).
pub fn prune(
    content: &str,
    user_query: &str,
    tokenizer: &Tokenizer,
    min_lines: usize,
) -> (String, Vec<CompressionEvent>) {
    let mut events = Vec::new();
    let tokens_before = tokenizer.count_tokens(content);

    let result = FENCED_BLOCK_RE.replace_all(content, |caps: &regex::Captures| {
        let full_match = caps.get(0).unwrap().as_str();
        let lang_tag = caps.get(1).unwrap().as_str();
        let code = caps.get(2).unwrap().as_str();

        // Only process blocks with enough lines
        let line_count = code.lines().count();
        if line_count < min_lines {
            return full_match.to_string();
        }

        // Detect language
        let lang = match detect_language(lang_tag) {
            Some(l) => l,
            None => return full_match.to_string(),
        };

        // Parse and prune
        match prune_code(code, lang, user_query, tokenizer) {
            Some(pruned) => {
                format!("```{}\n{}```", lang_tag, pruned)
            }
            None => full_match.to_string(),
        }
    });

    let result = result.to_string();
    let tokens_after = tokenizer.count_tokens(&result);

    if tokens_before > tokens_after {
        events.push(CompressionEvent {
            tokens_before,
            tokens_after,
            stage_name: "C_ast".to_string(),
            reason: "AST-aware function body pruning".to_string(),
            timestamp: std::time::Instant::now(),
        });
    }

    (result, events)
}

/// Parse code with tree-sitter and prune unreferenced function bodies
fn prune_code(
    code: &str,
    lang: &str,
    user_query: &str,
    tokenizer: &Tokenizer,
) -> Option<String> {
    let mut parser = tree_sitter::Parser::new();

    let language = match lang {
        "python" => tree_sitter_python::LANGUAGE,
        "javascript" => tree_sitter_javascript::LANGUAGE,
        "rust" => tree_sitter_rust::LANGUAGE,
        "go" => tree_sitter_go::LANGUAGE,
        _ => return None,
    };

    parser
        .set_language(&language.into())
        .ok()?;

    let tree = parser.parse(code, None)?;
    let root = tree.root_node();

    let query_lower = user_query.to_lowercase();

    // Collect function info
    let mut functions: Vec<FunctionInfo> = Vec::new();
    collect_functions(root, code, &query_lower, tokenizer, &mut functions);

    // If no functions found or all are referenced, no pruning needed
    if functions.is_empty() || functions.iter().all(|f| f.is_referenced) {
        return None;
    }

    // Build pruned output by replacing unreferenced function bodies
    let mut result = code.to_string();
    // Process in reverse order to maintain byte offsets
    functions.sort_by(|a, b| b.start_byte.cmp(&a.start_byte));

    // Filter out functions whose ranges are contained within another function's range
    // (e.g., methods inside impl blocks) to avoid overlapping replacements
    let mut filtered: Vec<&FunctionInfo> = Vec::new();
    for func in &functions {
        let dominated = functions.iter().any(|other| {
            std::ptr::eq(func, other) == false
                && other.start_byte <= func.start_byte
                && func.end_byte <= other.end_byte
        });
        if !dominated {
            filtered.push(func);
        }
    }

    for func in &filtered {
        if func.is_referenced {
            continue;
        }

        // Build stub replacement
        let stub = build_stub(
            &func.signature,
            func.body_token_count,
            lang,
        );

        // Replace the full function with signature + stub
        result.replace_range(func.start_byte..func.end_byte, &stub);
    }

    Some(result)
}

/// Collect top-level function/class definitions from tree-sitter AST
fn collect_functions(
    node: tree_sitter::Node,
    source: &str,
    query_lower: &str,
    tokenizer: &Tokenizer,
    out: &mut Vec<FunctionInfo>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        let kind = child.kind();

        let is_func = matches!(
            kind,
            "function_definition"       // Python
            | "function_declaration"     // JS, Go, Rust
            | "method_definition"        // JS class methods
            | "class_definition"         // Python
            | "class_declaration"        // JS
            | "function_item"            // Rust
            | "impl_item"               // Rust impl blocks
        );

        if !is_func {
            continue;
        }

        // Extract function name
        let name = extract_function_name(child, source);
        if name.is_empty() {
            continue;
        }

        let start = child.start_byte();
        let end = child.end_byte();

        // Find the body node
        let body_node = find_body_node(child);

        let (body_start, body_end, signature) = if let Some(body) = body_node {
            let sig_text = &source[start..body.start_byte()];
            (body.start_byte(), body.end_byte(), sig_text.trim_end().to_string())
        } else {
            // No body found — keep the function as-is
            continue;
        };

        let body_text = &source[body_start..body_end];
        let body_line_count = body_text.lines().count();

        // Only prune if body is substantial (3+ lines)
        if body_line_count < 3 {
            continue;
        }

        let body_token_count = tokenizer.count_tokens(body_text);

        // Check if referenced in user query (case-insensitive substring)
        let is_referenced = query_lower.contains(&name.to_lowercase());

        out.push(FunctionInfo {
            name,
            signature,
            start_byte: start,
            end_byte: end,
            body_token_count,
            is_referenced,
        });
    }
}

/// Extract function/class name from a tree-sitter node
fn extract_function_name(node: tree_sitter::Node, source: &str) -> String {
    // Look for a "name" or "identifier" child
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "name" || child.kind() == "identifier" {
            return source[child.start_byte()..child.end_byte()].to_string();
        }
        // Rust: function_item has name child
        if child.kind() == "type_identifier" {
            return source[child.start_byte()..child.end_byte()].to_string();
        }
    }
    String::new()
}

/// Find the body/block node of a function definition
fn find_body_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if matches!(
            kind,
            "block"
            | "statement_block"
            | "class_body"
            | "declaration_list"
            | "function_body"
        ) {
            return Some(child);
        }
    }
    None
}

/// Build a stub replacement for a pruned function
fn build_stub(
    signature: &str,
    body_tokens: usize,
    lang: &str,
) -> String {
    let comment = match lang {
        "python" => format!("{}    # [Janus: body pruned — {} tokens]", signature, body_tokens),
        "rust" => format!("{} {{\n    // [Janus: body pruned — {} tokens]\n}}", signature, body_tokens),
        "go" => format!("{} {{\n    // [Janus: body pruned — {} tokens]\n}}", signature, body_tokens),
        "javascript" => format!("{} {{\n    // [Janus: body pruned — {} tokens]\n}}", signature, body_tokens),
        _ => format!("{} // [Janus: body pruned — {} tokens]", signature, body_tokens),
    };
    comment
}
