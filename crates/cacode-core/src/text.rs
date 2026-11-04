//! UTF-8 safe string helpers and output clamping shared by every tool.
//!
//! Never slice a `&str` by a raw byte index coming from `len()`: always go
//! through these helpers so multi-byte text (Indonesian, emoji, box drawing)
//! can never panic.

/// Largest char boundary `<= idx`.
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= idx`.
pub fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Prefix of at most `max_bytes` bytes, cut on a char boundary.
pub fn prefix_bytes(s: &str, max_bytes: usize) -> &str {
    &s[..floor_char_boundary(s, max_bytes)]
}

/// Truncate to at most `max_chars` characters, appending `…` when cut.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((idx, _)) => format!("{}…", &s[..idx]),
    }
}

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 30_000;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 500;

/// Clamp tool output so a single observation can never blow the context
/// window. Keeps the head and the tail (errors usually live at the end) and
/// inserts an explicit marker describing what was dropped.
pub fn clamp_output(s: &str, max_bytes: usize, max_lines: usize) -> String {
    let total_lines = s.lines().count();
    if s.len() <= max_bytes && total_lines <= max_lines {
        return s.to_string();
    }

    // Line budget first.
    let lines: Vec<&str> = s.lines().collect();
    let (head_lines, tail_lines) = if total_lines > max_lines {
        let head = max_lines * 2 / 5;
        let tail = max_lines - head;
        (lines[..head].join("\n"), lines[total_lines - tail..].join("\n"))
    } else {
        (s.to_string(), String::new())
    };

    let joined_len = head_lines.len() + tail_lines.len();
    let (head, tail) = if joined_len > max_bytes {
        let head_budget = max_bytes * 2 / 5;
        let tail_budget = max_bytes - head_budget;
        if tail_lines.is_empty() {
            // single block: take head + tail of the same string
            let h = prefix_bytes(&head_lines, head_budget).to_string();
            let start = ceil_char_boundary(&head_lines, head_lines.len().saturating_sub(tail_budget));
            (h, head_lines[start..].to_string())
        } else {
            let h = prefix_bytes(&head_lines, head_budget).to_string();
            let start = ceil_char_boundary(&tail_lines, tail_lines.len().saturating_sub(tail_budget));
            (h, tail_lines[start..].to_string())
        }
    } else {
        (head_lines, tail_lines)
    };

    let dropped_bytes = s.len().saturating_sub(head.len() + tail.len());
    format!(
        "{}\n[... output truncated: {} bytes / {} total lines omitted from the middle ...]\n{}",
        head, dropped_bytes, total_lines, tail
    )
}

/// Default clamp used for tool observations.
pub fn clamp_default(s: &str) -> String {
    clamp_output(s, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_OUTPUT_LINES)
}

/// Rough token estimate (≈ 3.5 chars per token for mixed code / prose).
pub fn estimate_tokens(s: &str) -> i64 {
    ((s.chars().count() as f64) / 3.5).ceil() as i64
}

/// Detect binary content: NUL byte in the first 8 KiB.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multibyte_never_panics() {
        let s = format!("{{\"qq\":\"{}\"}}", "é".repeat(40));
        for n in 0..s.len() + 2 {
            let _ = prefix_bytes(&s, n);
        }
        assert_eq!(truncate_chars("ééé", 2), "éé…");
    }

    #[test]
    fn clamp_large_output_keeps_head_and_tail() {
        let big = format!("START\n{}\nEND_ERROR", "a".repeat(3_000_000));
        let out = clamp_default(&big);
        assert!(out.len() <= DEFAULT_MAX_OUTPUT_BYTES + 200, "len={}", out.len());
        assert!(out.starts_with("START"));
        assert!(out.ends_with("END_ERROR"));
        assert!(out.contains("output truncated"));
    }

    #[test]
    fn clamp_many_lines() {
        let s: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        let out = clamp_default(&s);
        assert!(out.lines().count() <= DEFAULT_MAX_OUTPUT_LINES + 2);
        assert!(out.contains("line 0"));
        assert!(out.contains("line 9999"));
    }

    #[test]
    fn clamp_multibyte_boundaries() {
        let s = "é".repeat(100_000);
        let out = clamp_output(&s, 1001, 10);
        assert!(out.contains("truncated"));
    }
}
