//! Human-output hygiene (external review 3rd round P1-4): strings that
//! originate outside portool -- worktree paths, branch names, labels --
//! can legally contain ANSI escapes, newlines, or wide characters. Every
//! human-readable surface routes them through [`sanitize`], and table
//! layout uses display [`width`] instead of byte length. JSON output is
//! untouched: serde escaping already makes it safe.

use unicode_width::UnicodeWidthStr;

/// Returns `s` safe for a terminal: `\n`, `\r`, `\t` become visible
/// two-character escapes, and every other control character (C0 + C1,
/// including ESC -- so no ANSI sequence survives) becomes U+FFFD.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// The terminal display width of `s` (wide CJK and emoji count as 2).
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// `s` left-justified to display width `w` (space-padded; never truncates).
pub fn pad(s: &str, w: usize) -> String {
    let deficit = w.saturating_sub(width(s));
    let mut out = String::with_capacity(s.len() + deficit);
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', deficit));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi_escapes() {
        assert_eq!(sanitize("a\u{1b}[31mred"), "a\u{FFFD}[31mred");
    }

    #[test]
    fn sanitize_makes_newlines_visible() {
        assert_eq!(sanitize("a\nb\rc\td"), "a\\nb\\rc\\td");
    }

    #[test]
    fn sanitize_replaces_c1_controls() {
        assert_eq!(sanitize("a\u{9b}b"), "a\u{FFFD}b"); // CSI (C1)
    }

    #[test]
    fn sanitize_keeps_plain_and_wide_text() {
        assert_eq!(sanitize("日本語 path/ブランチ"), "日本語 path/ブランチ");
    }

    #[test]
    fn width_counts_cjk_as_two_columns() {
        assert_eq!(width("日本語"), 6);
        assert_eq!(width("abc"), 3);
    }

    #[test]
    fn pad_uses_display_width() {
        assert_eq!(pad("日本", 6), "日本  ");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("toolong", 3), "toolong", "never truncates");
    }
}
