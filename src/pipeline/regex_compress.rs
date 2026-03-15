use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashSet;
use xxhash_rust::xxh3::xxh3_64;

use crate::metrics::CompressionEvent;
use crate::tokenizer::Tokenizer;

lazy_static! {
    // B1: Docstring removal
    // Python triple-quoted docstrings (after def/class or at module level)
    static ref PYTHON_DOCSTRING_RE: Regex = Regex::new(
        r#"(?m)((?:^|\n)\s*(?:def |class |async def )[^\n]*:\s*\n\s*)(?:"""[\s\S]*?"""|'''[\s\S]*?''')"#
    ).unwrap();
    // Standalone module-level triple-quoted docstrings
    static ref PYTHON_MODULE_DOCSTRING_RE: Regex = Regex::new(
        r#"(?m)^("""[\s\S]*?"""|'''[\s\S]*?''')\s*$"#
    ).unwrap();
    // JS/TS JSDoc blocks
    static ref JSDOC_RE: Regex = Regex::new(
        r"/\*\*[\s\S]*?\*/"
    ).unwrap();
    // Rust doc-comments (/// and //!)
    static ref RUST_DOC_COMMENT_RE: Regex = Regex::new(
        r"(?m)^\s*///.*$|^\s*//!.*$"
    ).unwrap();

    // B2: Comment stripping
    // Python line comments (but not shebangs, type: ignore, noqa, etc.)
    static ref PYTHON_COMMENT_RE: Regex = Regex::new(
        r"(?m)^(\s*)#(?!!/)(.*)$"
    ).unwrap();
    // JS/Rust/Go line comments
    static ref LINE_COMMENT_RE: Regex = Regex::new(
        r"(?m)^(\s*)//(?!/)(.*)$"
    ).unwrap();
    // Block comments (all /* ... */ including JSDoc — we filter JSDoc out in code)
    static ref BLOCK_COMMENT_RE: Regex = Regex::new(
        r"/\*[\s\S]*?\*/"
    ).unwrap();
    // Comments to preserve
    static ref PRESERVE_COMMENT_RE: Regex = Regex::new(
        r"(?i)type:\s*ignore|noqa|eslint-disable|@ts-ignore|TODO|FIXME|HACK|SAFETY|#!"
    ).unwrap();

    // B3: Whitespace normalization
    static ref MULTIPLE_BLANK_LINES_RE: Regex = Regex::new(
        r"\n\s*\n(\s*\n)+"
    ).unwrap();
    static ref TRAILING_WHITESPACE_RE: Regex = Regex::new(
        r"(?m)[ \t]+$"
    ).unwrap();
    static ref ZERO_WIDTH_RE: Regex = Regex::new(
        "[\u{FEFF}\u{200B}\u{200C}\u{200D}]"
    ).unwrap();

    // B4: Stack trace detection
    // Python traceback lines
    static ref PYTHON_TRACEBACK_LINE_RE: Regex = Regex::new(
        r#"^\s*File ".*", line \d+"#
    ).unwrap();
    // Node.js/JS stack trace lines
    static ref NODE_STACK_LINE_RE: Regex = Regex::new(
        r"^\s+at .+\(.+:\d+:\d+\)"
    ).unwrap();
    // Python traceback header
    static ref PYTHON_TRACEBACK_HEADER_RE: Regex = Regex::new(
        r"^Traceback \(most recent call last\):"
    ).unwrap();

    // B5: Fenced code block detection
    static ref FENCED_CODE_BLOCK_RE: Regex = Regex::new(
        r"(?ms)^```[^\n]*\n(.*?)^```"
    ).unwrap();
}

/// Run all Stage B compression sub-stages
pub fn compress(content: &str, tokenizer: &Tokenizer) -> (String, Vec<CompressionEvent>) {
    let mut result = content.to_string();
    let mut events = Vec::new();

    // B1: Docstring removal
    let (compressed, event) = remove_docstrings(&result, tokenizer);
    if let Some(e) = event {
        events.push(e);
    }
    result = compressed;

    // B2: Comment stripping
    let (compressed, event) = strip_comments(&result, tokenizer);
    if let Some(e) = event {
        events.push(e);
    }
    result = compressed;

    // B3: Whitespace normalization
    let (compressed, event) = normalize_whitespace(&result, tokenizer);
    if let Some(e) = event {
        events.push(e);
    }
    result = compressed;

    // B4: Stack trace truncation
    let (compressed, event) = truncate_stacktraces(&result, tokenizer);
    if let Some(e) = event {
        events.push(e);
    }
    result = compressed;

    // B5: Duplicate code block collapse
    let (compressed, event) = collapse_duplicate_blocks(&result, tokenizer);
    if let Some(e) = event {
        events.push(e);
    }
    result = compressed;

    (result, events)
}

/// B1: Remove docstrings from Python, JS/TS, and Rust code
fn remove_docstrings(content: &str, tokenizer: &Tokenizer) -> (String, Option<CompressionEvent>) {
    let tokens_before = tokenizer.count_tokens(content);

    let mut result = PYTHON_DOCSTRING_RE.replace_all(content, "$1").to_string();
    result = PYTHON_MODULE_DOCSTRING_RE.replace_all(&result, "").to_string();
    result = JSDOC_RE.replace_all(&result, "").to_string();
    result = RUST_DOC_COMMENT_RE.replace_all(&result, "").to_string();

    let tokens_after = tokenizer.count_tokens(&result);

    let event = if tokens_before > tokens_after {
        Some(CompressionEvent {
            tokens_before,
            tokens_after,
            stage_name: "B1_docstrings".to_string(),
            reason: "Removed docstrings and doc-comments".to_string(),
            timestamp: std::time::Instant::now(),
        })
    } else {
        None
    };

    (result, event)
}

/// B2: Strip inline comments (preserving shebangs, type: ignore, etc.)
fn strip_comments(content: &str, tokenizer: &Tokenizer) -> (String, Option<CompressionEvent>) {
    let tokens_before = tokenizer.count_tokens(content);

    let mut result = String::new();
    for line in content.lines() {
        let trimmed = line.trim();

        // Check if line has a comment that should be preserved
        if PRESERVE_COMMENT_RE.is_match(line) {
            result.push_str(line);
            result.push('\n');
            continue;
        }

        // Remove Python-style comments
        if let Some(pos) = find_comment_start(trimmed, '#') {
            // Only if the entire line is a comment (starts with #)
            if trimmed.starts_with('#') {
                // Skip pure comment lines entirely
                result.push('\n');
                continue;
            }
            // Inline comment: find the comment position in the original line
            let leading_ws = line.len() - line.trim_start().len();
            let code_part = &line[..leading_ws + pos];
            result.push_str(code_part.trim_end());
            result.push('\n');
            continue;
        }

        // Remove // line comments (but not URLs like http://)
        if let Some(pos) = find_double_slash_comment(trimmed) {
            if pos == 0 {
                result.push('\n');
                continue;
            }
            // Find the comment position in the original line
            let leading_ws = line.len() - line.trim_start().len();
            let code_part = &line[..leading_ws + pos];
            result.push_str(code_part.trim_end());
            result.push('\n');
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    // Remove block comments but keep JSDoc (/** ... */) since B1 handles those
    result = BLOCK_COMMENT_RE.replace_all(&result, |caps: &regex::Captures| {
        let matched = caps.get(0).unwrap().as_str();
        if matched.starts_with("/**") {
            // Keep JSDoc blocks — they're handled by B1
            matched.to_string()
        } else {
            String::new()
        }
    }).to_string();

    // Trim trailing newline to match input
    if !content.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }

    let tokens_after = tokenizer.count_tokens(&result);

    let event = if tokens_before > tokens_after {
        Some(CompressionEvent {
            tokens_before,
            tokens_after,
            stage_name: "B2_comments".to_string(),
            reason: "Stripped inline comments".to_string(),
            timestamp: std::time::Instant::now(),
        })
    } else {
        None
    };

    (result, event)
}

/// Find position of a comment character, ignoring those inside strings
fn find_comment_start(line: &str, comment_char: char) -> Option<usize> {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        } else if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        } else if ch == comment_char && !in_single_quote && !in_double_quote {
            return Some(i);
        }
    }
    None
}

/// Find position of // comment, ignoring URLs (http://, https://)
fn find_double_slash_comment(line: &str) -> Option<usize> {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut prev_char: Option<char> = None;

    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            prev_char = Some(ch);
            continue;
        }
        if ch == '\\' {
            escaped = true;
            prev_char = Some(ch);
            continue;
        }
        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        } else if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        } else if ch == '/' && !in_single_quote && !in_double_quote {
            // Look ahead for another /
            if line[i..].starts_with("//") {
                // Check it's not preceded by : (URL like http://)
                if prev_char == Some(':') {
                    prev_char = Some(ch);
                    continue;
                }
                return Some(i);
            }
        }
        prev_char = Some(ch);
    }
    None
}

/// B3: Normalize whitespace
fn normalize_whitespace(content: &str, tokenizer: &Tokenizer) -> (String, Option<CompressionEvent>) {
    let tokens_before = tokenizer.count_tokens(content);

    let mut result = ZERO_WIDTH_RE.replace_all(content, "").to_string();
    result = TRAILING_WHITESPACE_RE.replace_all(&result, "").to_string();
    result = MULTIPLE_BLANK_LINES_RE.replace_all(&result, "\n\n").to_string();

    let tokens_after = tokenizer.count_tokens(&result);

    let event = if tokens_before > tokens_after {
        Some(CompressionEvent {
            tokens_before,
            tokens_after,
            stage_name: "B3_whitespace".to_string(),
            reason: "Normalized whitespace".to_string(),
            timestamp: std::time::Instant::now(),
        })
    } else {
        None
    };

    (result, event)
}

/// B4: Truncate stack traces (keep first 5 + last 5 lines)
fn truncate_stacktraces(content: &str, tokenizer: &Tokenizer) -> (String, Option<CompressionEvent>) {
    let tokens_before = tokenizer.count_tokens(content);
    let lines: Vec<&str> = content.lines().collect();

    let mut result_lines: Vec<String> = Vec::new();
    let mut i = 0;
    let mut modified = false;

    while i < lines.len() {
        // Check for Python traceback header
        if PYTHON_TRACEBACK_HEADER_RE.is_match(lines[i]) {
            let start = i;
            i += 1;
            // Collect all traceback lines
            while i < lines.len()
                && (PYTHON_TRACEBACK_LINE_RE.is_match(lines[i])
                    || lines[i].starts_with("    ")
                    || lines[i].is_empty())
            {
                i += 1;
            }
            // Include the error line
            if i < lines.len() {
                i += 1;
            }
            let trace_lines: Vec<&str> = lines[start..i].to_vec();
            if trace_lines.len() > 10 {
                modified = true;
                let omitted = trace_lines.len() - 10;
                for line in &trace_lines[..5] {
                    result_lines.push(line.to_string());
                }
                result_lines.push(format!("[... {} frames omitted by Janus ...]", omitted));
                for line in &trace_lines[trace_lines.len() - 5..] {
                    result_lines.push(line.to_string());
                }
            } else {
                for line in &trace_lines {
                    result_lines.push(line.to_string());
                }
            }
            continue;
        }

        // Check for Node.js stack trace
        if NODE_STACK_LINE_RE.is_match(lines[i]) {
            let start = i;
            while i < lines.len() && NODE_STACK_LINE_RE.is_match(lines[i]) {
                i += 1;
            }
            let trace_lines: Vec<&str> = lines[start..i].to_vec();
            if trace_lines.len() > 10 {
                modified = true;
                let omitted = trace_lines.len() - 10;
                for line in &trace_lines[..5] {
                    result_lines.push(line.to_string());
                }
                result_lines.push(format!("[... {} frames omitted by Janus ...]", omitted));
                for line in &trace_lines[trace_lines.len() - 5..] {
                    result_lines.push(line.to_string());
                }
            } else {
                for line in &trace_lines {
                    result_lines.push(line.to_string());
                }
            }
            continue;
        }

        result_lines.push(lines[i].to_string());
        i += 1;
    }

    if !modified {
        return (content.to_string(), None);
    }

    let result = result_lines.join("\n");
    let tokens_after = tokenizer.count_tokens(&result);

    let event = if tokens_before > tokens_after {
        Some(CompressionEvent {
            tokens_before,
            tokens_after,
            stage_name: "B4_stacktrace".to_string(),
            reason: "Truncated stack traces".to_string(),
            timestamp: std::time::Instant::now(),
        })
    } else {
        None
    };

    (result, event)
}

/// B5: Collapse duplicate fenced code blocks
fn collapse_duplicate_blocks(content: &str, tokenizer: &Tokenizer) -> (String, Option<CompressionEvent>) {
    let tokens_before = tokenizer.count_tokens(content);
    let mut seen_hashes: HashSet<u64> = HashSet::new();
    let mut modified = false;

    let result = FENCED_CODE_BLOCK_RE.replace_all(content, |caps: &regex::Captures| {
        let full_match = caps.get(0).unwrap().as_str();
        let block_content = caps.get(1).unwrap().as_str();

        // Normalize whitespace before hashing
        let normalized = block_content.trim();
        let hash = xxh3_64(normalized.as_bytes());

        if seen_hashes.contains(&hash) {
            modified = true;
            let block_tokens = tokenizer.count_tokens(block_content);
            format!("[Janus: duplicate of code block above — {} tokens omitted]", block_tokens)
        } else {
            seen_hashes.insert(hash);
            full_match.to_string()
        }
    });

    if !modified {
        return (content.to_string(), None);
    }

    let result = result.to_string();
    let tokens_after = tokenizer.count_tokens(&result);

    let event = if tokens_before > tokens_after {
        Some(CompressionEvent {
            tokens_before,
            tokens_after,
            stage_name: "B5_dedup_blocks".to_string(),
            reason: "Collapsed duplicate code blocks".to_string(),
            timestamp: std::time::Instant::now(),
        })
    } else {
        None
    };

    (result, event)
}
