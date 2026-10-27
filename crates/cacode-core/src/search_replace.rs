use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// A parsed search/replace block from an LLM response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchReplaceBlock {
    pub file_path: Option<String>,
    pub search: String,
    pub replace: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ParseState {
    Outside,
    InSearch,
    InReplace,
}

/// Helper to validate if a trimmed string looks like a file path.
fn is_valid_file_path(s: &str) -> bool {
    if s.is_empty() || s.contains(' ') || s.contains('\t') || s.starts_with("```") {
        return false;
    }
    if s == "true" || s == "false" || s == "null" || s == "None" {
        return false;
    }
    if s.contains('/') || s.contains('\\') {
        return true;
    }
    if let Some((_, ext)) = s.rsplit_once('.') {
        if !ext.is_empty() && ext.len() <= 10 && !s.starts_with('.') {
            return true;
        }
    }
    matches!(s, "Makefile" | "Dockerfile" | "Containerfile" | "Gemfile" | "Rakefile")
}

/// Extracts a candidate file path from a line preceding a search/replace block.
fn extract_file_path(line: &str) -> Option<String> {
    let mut s = line.trim();
    if s.is_empty() || s.starts_with("```") {
        return None;
    }

    // Strip markdown headers like ###, ##, #
    s = s.trim_start_matches('#').trim();

    // Strip markdown bold / italic
    if s.starts_with("**") && s.ends_with("**") && s.len() >= 4 {
        s = &s[2..s.len() - 2];
        s = s.trim();
    } else if s.starts_with('*') && s.ends_with('*') && s.len() >= 2 {
        s = &s[1..s.len() - 1];
        s = s.trim();
    }

    // Strip inline backticks `...`
    if s.starts_with('`') && s.ends_with('`') && s.len() >= 2 {
        s = &s[1..s.len() - 1];
        s = s.trim();
    }

    // Strip prefixes like "File:", "FilePath:", "Path:", "Filename:", "In:"
    let lower = s.to_lowercase();
    for prefix in &["file:", "filepath:", "path:", "filename:", "in:"] {
        if lower.starts_with(prefix) {
            s = s[prefix.len()..].trim();
            break;
        }
    }

    // Strip trailing colon
    s = s.trim_end_matches(':').trim();

    // Strip surrounding quotes
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        if s.len() >= 2 {
            s = &s[1..s.len() - 1];
            s = s.trim();
        }
    }

    // Re-check backticks
    if s.starts_with('`') && s.ends_with('`') && s.len() >= 2 {
        s = &s[1..s.len() - 1];
        s = s.trim();
    }

    if s.is_empty() || s.starts_with("```") {
        return None;
    }

    if s.contains(' ') || s.contains('\t') {
        // Check if backticks enclose a path within the line, e.g. "In `src/lib.rs`:"
        if let Some(start) = s.find('`') {
            if let Some(end) = s[start + 1..].find('`') {
                let candidate = &s[start + 1..start + 1 + end];
                if is_valid_file_path(candidate) {
                    return Some(candidate.to_string());
                }
            }
        }
        return None;
    }

    if is_valid_file_path(s) {
        Some(s.to_string())
    } else {
        None
    }
}

/// Parses Aider-style search/replace blocks from text.
///
/// Supports:
/// - Exact `<<<<<<< SEARCH` ... `=======` ... `>>>>>>> REPLACE`
/// - Preceding file path (plain line, markdown header, bold, or backticked)
/// - Blocks enclosed in markdown code fences (```...```)
/// - File path specified directly on the `<<<<<<< SEARCH: file.rs` line
/// - Multi-block sequences inheriting the previous file path
pub fn parse_search_replace_blocks(text: &str) -> Vec<SearchReplaceBlock> {
    let mut blocks = Vec::new();
    let mut state = ParseState::Outside;
    let mut current_file_path: Option<String> = None;
    let mut last_file_path: Option<String> = None;
    let mut recent_lines: Vec<String> = Vec::new();
    let mut search_lines: Vec<String> = Vec::new();
    let mut replace_lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();

        let is_search_marker = trimmed.starts_with("<<<<<<<") && {
            let after = trimmed[7..].trim_start();
            after.to_uppercase().starts_with("SEARCH")
        };

        if is_search_marker {
            // Check if file path is on the search marker line itself
            let after = trimmed[7..].trim_start();
            let mut rest = if after.len() >= 6 && after[..6].eq_ignore_ascii_case("search") {
                after[6..].trim()
            } else {
                ""
            };
            rest = rest.trim_start_matches(':').trim();
            let path_on_marker = if !rest.is_empty() {
                extract_file_path(rest)
            } else {
                None
            };

            if let Some(p) = path_on_marker {
                current_file_path = Some(p.clone());
                last_file_path = Some(p);
            } else {
                // Look back through recent lines before SEARCH marker
                let mut found_path = None;
                for prev in recent_lines.iter().rev() {
                    let prev_trimmed = prev.trim();
                    if prev_trimmed.is_empty() || prev_trimmed.starts_with("```") {
                        continue;
                    }
                    if let Some(p) = extract_file_path(prev_trimmed) {
                        found_path = Some(p);
                    }
                    break;
                }

                if let Some(p) = found_path {
                    current_file_path = Some(p.clone());
                    last_file_path = Some(p);
                } else if last_file_path.is_some() {
                    current_file_path = last_file_path.clone();
                } else {
                    current_file_path = None;
                }
            }

            search_lines.clear();
            replace_lines.clear();
            state = ParseState::InSearch;
            continue;
        }

        match state {
            ParseState::Outside => {
                recent_lines.push(line.to_string());
                if recent_lines.len() > 10 {
                    recent_lines.remove(0);
                }
            }
            ParseState::InSearch => {
                let is_sep = trimmed.starts_with("=======")
                    && (trimmed.chars().all(|c| c == '=')
                        || trimmed.len() == 7
                        || trimmed.chars().nth(7) == Some(' '));

                if is_sep {
                    state = ParseState::InReplace;
                } else {
                    search_lines.push(line.to_string());
                }
            }
            ParseState::InReplace => {
                let is_replace_marker = trimmed.starts_with(">>>>>>>") && {
                    let after = trimmed[7..].trim_start();
                    after.to_uppercase().starts_with("REPLACE")
                };

                if is_replace_marker {
                    let search_str = if search_lines.is_empty() {
                        String::new()
                    } else {
                        format!("{}\n", search_lines.join("\n"))
                    };

                    let replace_str = if replace_lines.is_empty() {
                        String::new()
                    } else {
                        format!("{}\n", replace_lines.join("\n"))
                    };

                    blocks.push(SearchReplaceBlock {
                        file_path: current_file_path.take(),
                        search: search_str,
                        replace: replace_str,
                    });

                    state = ParseState::Outside;
                    recent_lines.clear();
                } else {
                    replace_lines.push(line.to_string());
                }
            }
        }
    }

    blocks
}

/// Compute Levenshtein similarity between two strings (0.0 to 1.0).
fn str_similarity(s1: &str, s2: &str) -> f64 {
    if s1 == s2 {
        return 1.0;
    }
    if s1.is_empty() || s2.is_empty() {
        return 0.0;
    }
    let s1_chars: Vec<char> = s1.chars().collect();
    let s2_chars: Vec<char> = s2.chars().collect();
    let m = s1_chars.len();
    let n = s2_chars.len();
    let max_len = m.max(n);
    if max_len == 0 {
        return 1.0;
    }

    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if s1_chars[i - 1] == s2_chars[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        prev.copy_from_slice(&curr);
    }

    let dist = prev[n];
    1.0 - (dist as f64 / max_len as f64)
}

/// Replaces lines in `norm_content` from `start_line` for `count` lines with `norm_replace`,
/// adjusting indentation by `shift` and formatting line endings.
fn replace_line_range(
    norm_content: &str,
    c_chunks: &[&str],
    start_line: usize,
    count: usize,
    norm_replace: &str,
    shift: isize,
    is_crlf: bool,
) -> String {
    let start_byte: usize = c_chunks[..start_line].iter().map(|c| c.len()).sum();
    let end_byte: usize = c_chunks[..start_line + count].iter().map(|c| c.len()).sum();

    let mut adjusted_replace = if shift != 0 {
        let mut lines = Vec::new();
        for line in norm_replace.lines() {
            if line.trim().is_empty() {
                lines.push(String::new());
            } else if shift > 0 {
                let pad = " ".repeat(shift as usize);
                lines.push(format!("{}{}", pad, line));
            } else {
                let remove = (-shift) as usize;
                let leading = line.len() - line.trim_start().len();
                let actual = remove.min(leading);
                lines.push(line[actual..].to_string());
            }
        }
        lines.join("\n")
    } else {
        norm_replace.to_string()
    };

    if !adjusted_replace.is_empty() {
        let had_nl = norm_replace.ends_with('\n')
            || (end_byte > 0 && norm_content[..end_byte].ends_with('\n'));
        if had_nl && !adjusted_replace.ends_with('\n') {
            adjusted_replace.push('\n');
        }
    }

    let mut result = String::with_capacity(norm_content.len() + adjusted_replace.len());
    result.push_str(&norm_content[..start_byte]);
    result.push_str(&adjusted_replace);
    result.push_str(&norm_content[end_byte..]);

    if is_crlf {
        result.replace('\n', "\r\n")
    } else {
        result
    }
}

/// Applies a single search/replace block to `content`.
///
/// Executes matching strategies in order:
/// 1. Exact string match
/// 2. Whitespace-flexible match (CRLF/LF normalization and trailing whitespace trimming)
/// 3. Indentation-tolerant matching (uniform shift)
/// 4. Fallback line-window fuzzy matching
pub fn apply_search_replace(content: &str, search: &str, replace: &str) -> Result<String> {
    if search.is_empty() {
        if content.is_empty() {
            return Ok(replace.to_string());
        }
        return Ok(format!("{}{}", replace, content));
    }

    // 1. Exact string match
    if content.contains(search) {
        return Ok(content.replacen(search, replace, 1));
    }

    let is_crlf = content.contains("\r\n");
    let norm_content = content.replace("\r\n", "\n");
    let norm_search = search.replace("\r\n", "\n");
    let norm_replace = replace.replace("\r\n", "\n");

    // 2a. CRLF-normalized exact match
    if norm_content.contains(&norm_search) {
        let res = norm_content.replacen(&norm_search, &norm_replace, 1);
        return Ok(if is_crlf {
            res.replace('\n', "\r\n")
        } else {
            res
        });
    }

    let c_chunks: Vec<&str> = norm_content.split_inclusive('\n').collect();
    let c_lines: Vec<&str> = norm_content.lines().collect();
    let s_lines: Vec<&str> = norm_search.lines().collect();

    if s_lines.is_empty() {
        return Err(anyhow!("Search content has no lines"));
    }

    let m = s_lines.len();

    // 2b. Line-based match with trailing whitespace trimmed per line
    if c_lines.len() >= m {
        for i in 0..=(c_lines.len() - m) {
            let mut matches = true;
            for k in 0..m {
                if c_lines[i + k].trim_end() != s_lines[k].trim_end() {
                    matches = false;
                    break;
                }
            }
            if matches {
                return Ok(replace_line_range(
                    &norm_content,
                    &c_chunks,
                    i,
                    m,
                    &norm_replace,
                    0,
                    is_crlf,
                ));
            }
        }
    }

    // 3. Indentation-tolerant matching (uniform relative indentation shift)
    if c_lines.len() >= m {
        for i in 0..=(c_lines.len() - m) {
            let mut trimmed_match = true;
            for k in 0..m {
                if c_lines[i + k].trim() != s_lines[k].trim() {
                    trimmed_match = false;
                    break;
                }
            }
            if trimmed_match {
                if let Some(first_non_empty) = s_lines.iter().position(|l| !l.trim().is_empty()) {
                    let s_indent = s_lines[first_non_empty].len()
                        - s_lines[first_non_empty].trim_start().len();
                    let c_indent = c_lines[i + first_non_empty].len()
                        - c_lines[i + first_non_empty].trim_start().len();
                    let shift = c_indent as isize - s_indent as isize;

                    let mut uniform = true;
                    for k in 0..m {
                        if s_lines[k].trim().is_empty() {
                            continue;
                        }
                        let s_k_indent = s_lines[k].len() - s_lines[k].trim_start().len();
                        let c_k_indent = c_lines[i + k].len() - c_lines[i + k].trim_start().len();
                        if c_k_indent as isize - s_k_indent as isize != shift {
                            uniform = false;
                            break;
                        }
                    }

                    if uniform {
                        return Ok(replace_line_range(
                            &norm_content,
                            &c_chunks,
                            i,
                            m,
                            &norm_replace,
                            shift,
                            is_crlf,
                        ));
                    }
                }
            }
        }
    }

    // 4. Fallback line-window fuzzy match
    if c_lines.len() < m {
        return Err(anyhow!(
            "Search block ({} lines) is longer than target content ({} lines)",
            m,
            c_lines.len()
        ));
    }

    let mut best_index = None;
    let mut best_score = 0.0;
    for i in 0..=(c_lines.len() - m) {
        let mut total_score = 0.0;
        for k in 0..m {
            let s = s_lines[k].trim();
            let c = c_lines[i + k].trim();
            if s == c {
                total_score += 1.0;
            } else {
                total_score += str_similarity(s, c);
            }
        }
        let score = total_score / m as f64;
        if score > best_score {
            best_score = score;
            best_index = Some(i);
        }
    }

    if best_score >= 0.70 {
        if let Some(best_i) = best_index {
            let shift = if let Some(first_idx) = s_lines.iter().position(|l| !l.trim().is_empty()) {
                let s_ind = s_lines[first_idx].len() - s_lines[first_idx].trim_start().len();
                let c_ind = c_lines[best_i + first_idx].len()
                    - c_lines[best_i + first_idx].trim_start().len();
                c_ind as isize - s_ind as isize
            } else {
                0
            };

            return Ok(replace_line_range(
                &norm_content,
                &c_chunks,
                best_i,
                m,
                &norm_replace,
                shift,
                is_crlf,
            ));
        }
    }

    let snippet: String = search.lines().take(4).collect::<Vec<_>>().join("\n");
    Err(anyhow!(
        "Search block not found in content (best fuzzy match: {:.1}%, threshold: 70%). Snippet:\n{}",
        best_score * 100.0,
        snippet
    ))
}

/// Applies a series of search/replace blocks to a specific file on disk.
pub fn apply_blocks_to_file(path: &Path, blocks: &[SearchReplaceBlock]) -> Result<String> {
    if blocks.is_empty() {
        return Ok(format!("No search/replace blocks to apply to {}", path.display()));
    }

    let mut content = if path.exists() {
        fs::read_to_string(path)
            .map_err(|e| anyhow!("Failed to read {}: {}", path.display(), e))?
    } else if blocks.iter().all(|b| b.search.trim().is_empty()) {
        String::new()
    } else {
        return Err(anyhow!("File does not exist: {}", path.display()));
    };

    for (idx, block) in blocks.iter().enumerate() {
        content = apply_search_replace(&content, &block.search, &block.replace)
            .map_err(|e| anyhow!("Failed applying block #{} to {}: {}", idx + 1, path.display(), e))?;
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow!("Failed to create directory {}: {}", parent.display(), e))?;
        }
    }

    fs::write(path, &content)
        .map_err(|e| anyhow!("Failed to write {}: {}", path.display(), e))?;

    Ok(content)
}

/// Parses all search/replace blocks from `text` and applies them to their respective files.
/// Returns the list of modified file paths.
pub fn apply_all_in_text(text: &str, base_dir: Option<&Path>) -> Result<Vec<String>> {
    let blocks = parse_search_replace_blocks(text);
    if blocks.is_empty() {
        return Ok(Vec::new());
    }

    let mut file_order: Vec<String> = Vec::new();
    let mut file_blocks: HashMap<String, Vec<SearchReplaceBlock>> = HashMap::new();

    for block in blocks {
        let path_str = block.file_path.clone().ok_or_else(|| {
            anyhow!(
                "Search/replace block missing target file path. Snippet: {}",
                block.search.lines().take(2).collect::<Vec<_>>().join(" ")
            )
        })?;

        if !file_blocks.contains_key(&path_str) {
            file_order.push(path_str.clone());
        }
        file_blocks.entry(path_str).or_default().push(block);
    }

    for path_str in &file_order {
        let full_path = if let Some(base) = base_dir {
            base.join(path_str)
        } else {
            std::path::PathBuf::from(path_str)
        };
        let b = &file_blocks[path_str];
        apply_blocks_to_file(&full_path, b)?;
    }

    Ok(file_order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match_replacement() {
        let content = "fn foo() {\n    let a = 1;\n    let b = 2;\n}\n";
        let search = "    let a = 1;\n";
        let replace = "    let a = 100;\n";
        let result = apply_search_replace(content, search, replace).unwrap();
        assert_eq!(result, "fn foo() {\n    let a = 100;\n    let b = 2;\n}\n");
    }

    #[test]
    fn test_multiline_replacement() {
        let content = "line 1\nline 2\nline 3\nline 4\n";
        let search = "line 2\nline 3\n";
        let replace = "line TWO\nline THREE\nline THREE_B\n";
        let result = apply_search_replace(content, search, replace).unwrap();
        assert_eq!(result, "line 1\nline TWO\nline THREE\nline THREE_B\nline 4\n");

        // Deletion
        let result_del = apply_search_replace(content, search, "").unwrap();
        assert_eq!(result_del, "line 1\nline 4\n");
    }

    #[test]
    fn test_whitespace_and_line_ending_tolerance() {
        // CRLF in content, LF in search
        let content_crlf = "fn foo() {\r\n    let x = 10;\r\n}\r\n";
        let search_lf = "    let x = 10;\n";
        let replace_lf = "    let x = 20;\n";
        let result = apply_search_replace(content_crlf, search_lf, replace_lf).unwrap();
        assert_eq!(result, "fn foo() {\r\n    let x = 20;\r\n}\r\n");

        // Trailing whitespace tolerance
        let content_trailing = "fn foo()   \n    let x = 10;  \n}\n";
        let search_clean = "fn foo()\n    let x = 10;\n";
        let replace_clean = "fn foo()\n    let x = 99;\n";
        let result2 = apply_search_replace(content_trailing, search_clean, replace_clean).unwrap();
        assert_eq!(result2, "fn foo()\n    let x = 99;\n}\n");
    }

    #[test]
    fn test_indentation_shift_positive() {
        // Content indented by 8 spaces
        let content = "        if cond {\n            action();\n        }\n";
        // Search block indented by 4 spaces
        let search = "    if cond {\n        action();\n    }\n";
        // Replace block indented by 4 spaces
        let replace = "    if cond {\n        action_v2();\n        notify();\n    }\n";

        let result = apply_search_replace(content, search, replace).unwrap();
        let expected = "        if cond {\n            action_v2();\n            notify();\n        }\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_indentation_shift_negative() {
        // Content indented by 0 spaces
        let content = "if cond {\n    action();\n}\n";
        // Search block indented by 4 spaces
        let search = "    if cond {\n        action();\n    }\n";
        // Replace block indented by 4 spaces
        let replace = "    if cond {\n        action_v2();\n    }\n";

        let result = apply_search_replace(content, search, replace).unwrap();
        let expected = "if cond {\n    action_v2();\n}\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_fallback_line_window_fuzzy_match() {
        let content = "fn calculate() {\n    // compute total tokens count\n    let total = prompt + comp;\n    println!(\"{}\", total);\n}\n";
        // Search snippet with slight comment difference
        let search = "    // compute token count\n    let total = prompt + comp;\n    println!(\"{}\", total);\n";
        let replace = "    let total = prompt + comp;\n    info!(\"tokens: {}\", total);\n";

        let result = apply_search_replace(content, search, replace).unwrap();
        assert!(result.contains("info!(\"tokens: {}\", total);"));
        assert!(!result.contains("compute total tokens count"));
    }

    #[test]
    fn test_proper_error_reporting_when_not_found() {
        let content = "fn foo() {\n    let a = 1;\n}\n";
        let search = "fn completely_different() {\n    unknown_call();\n}\n";
        let replace = "fn bar() {}\n";

        let err = apply_search_replace(content, search, replace).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Search block not found in content"));
    }

    #[test]
    fn test_parse_search_replace_blocks_basic() {
        let text = r#"
Here is the change:

src/calc.rs
<<<<<<< SEARCH
fn add(a: i32, b: i32) -> i32 {
    a - b
}
=======
fn add(a: i32, b: i32) -> i32 {
    a + b
}
>>>>>>> REPLACE
"#;

        let blocks = parse_search_replace_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].file_path, Some("src/calc.rs".to_string()));
        assert_eq!(blocks[0].search, "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n");
        assert_eq!(blocks[0].replace, "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
    }

    #[test]
    fn test_parse_search_replace_in_markdown_code_fences() {
        let text = r#"
```rust
crates/cacode-core/src/lib.rs
<<<<<<< SEARCH
pub mod commands;
=======
pub mod commands;
pub mod search_replace;
>>>>>>> REPLACE
```
"#;

        let blocks = parse_search_replace_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].file_path, Some("crates/cacode-core/src/lib.rs".to_string()));
        assert_eq!(blocks[0].search, "pub mod commands;\n");
        assert_eq!(blocks[0].replace, "pub mod commands;\npub mod search_replace;\n");
    }

    #[test]
    fn test_multiple_search_replace_blocks_and_inheritance() {
        let text = r#"
crates/cacode-core/src/lib.rs
<<<<<<< SEARCH
pub mod commands;
=======
pub mod commands;
pub mod sr;
>>>>>>> REPLACE

<<<<<<< SEARCH
pub mod config;
=======
pub mod config;
pub use sr::*;
>>>>>>> REPLACE

other/path.rs
<<<<<<< SEARCH
old_func()
=======
new_func()
>>>>>>> REPLACE
"#;

        let blocks = parse_search_replace_blocks(text);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].file_path, Some("crates/cacode-core/src/lib.rs".to_string()));
        assert_eq!(blocks[1].file_path, Some("crates/cacode-core/src/lib.rs".to_string()));
        assert_eq!(blocks[2].file_path, Some("other/path.rs".to_string()));
    }

    #[test]
    fn test_apply_blocks_to_file_and_apply_all_in_text() {
        let temp_dir = std::env::temp_dir().join(format!("cacode_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&temp_dir).unwrap();

        let file_a = temp_dir.join("file_a.txt");
        fs::write(&file_a, "alpha\nbravo\ncharlie\n").unwrap();

        let file_b = temp_dir.join("file_b.txt");
        fs::write(&file_b, "one\ntwo\nthree\n").unwrap();

        let text = format!(
            "{}\n<<<<<<< SEARCH\nbravo\n=======\nBRAVO_MOD\n>>>>>>> REPLACE\n\n{}\n<<<<<<< SEARCH\ntwo\n=======\nTWO_MOD\n>>>>>>> REPLACE\n",
            file_a.display(),
            file_b.display()
        );

        let modified = apply_all_in_text(&text, None).unwrap();
        assert_eq!(modified.len(), 2);

        let content_a = fs::read_to_string(&file_a).unwrap();
        assert_eq!(content_a, "alpha\nBRAVO_MOD\ncharlie\n");

        let content_b = fs::read_to_string(&file_b).unwrap();
        assert_eq!(content_b, "one\nTWO_MOD\nthree\n");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
