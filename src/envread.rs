//! Reading and expanding user-supplied env files for `portool exec`
//! (spec v0.4 §7–§8).
//!
//! Parsing and expansion are split: [`parse_env_file`] turns one file into
//! an ordered list of `(name, value)` pairs (duplicate keys are preserved;
//! last-wins merging is the caller's job), and [`resolve`] takes the final
//! precedence-merged map and expands `${NAME}` / `${NAME:-default}`
//! references. Error messages carry `path:line` origins but never echo
//! values, so secrets in env files cannot leak through diagnostics.

use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// How a value was quoted in its env file, which decides whether it is
/// subject to variable expansion: unquoted and double-quoted values are
/// expanded, single-quoted values are taken literally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quoting {
    None,
    Single,
    Double,
}

/// A single value read from an env file, before expansion.
#[derive(Debug, Clone)]
pub struct FileValue {
    pub raw: String,
    pub quoting: Quoting,
    /// "path:line" — エラーメッセージ用
    pub origin: String,
}

/// One entry in the precedence-merged variable map handed to [`resolve`].
#[derive(Debug, Clone)]
pub enum Entry {
    /// env ファイル由来。quoting に応じて展開対象になる
    File(FileValue),
    /// 親環境や portool 由来。常にリテラル（展開しない）
    Literal(String),
}

/// 1つの env ファイルをパースして (name, value) の出現順 Vec を返す。
/// 重複キーの後勝ち処理は呼び出し側が行う。
pub fn parse_env_file(path: &Path) -> Result<Vec<(String, FileValue)>> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::General(format!(
                "env file not found: {}",
                path.display()
            )));
        }
        Err(err) => return Err(err.into()),
    };
    parse_env_str(&source, &path.display().to_string())
}

/// Parses env file source text. Lines are split on `\n` with a trailing
/// `\r` stripped (CRLF support); blank lines and `#` comment lines are
/// skipped; everything else must be `KEY=VALUE`. Values may be unquoted
/// (trimmed, inline comments are not interpreted), single-quoted (no
/// escapes), or double-quoted (only `\\` and `\"` are unescaped).
fn parse_env_str(source: &str, display_path: &str) -> Result<Vec<(String, FileValue)>> {
    let mut entries = Vec::new();

    for (idx, line) in source.split('\n').enumerate() {
        let line_no = idx + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Never include the line's content in errors: it may hold secrets.
        let invalid_line = || {
            Error::General(format!(
                "{display_path}:{line_no}: invalid env line (expected KEY=VALUE)"
            ))
        };

        // KEY: [A-Za-z_][A-Za-z0-9_]*, optionally followed by whitespace,
        // then '='. The key charset is ASCII, so byte indexing is safe.
        let bytes = trimmed.as_bytes();
        if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
            return Err(invalid_line());
        }
        let mut i = 1;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let key = &trimmed[..i];
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            return Err(invalid_line());
        }
        let value_part = trimmed[i + 1..].trim_start();

        let unclosed = || {
            Error::General(format!(
                "{display_path}:{line_no}: unclosed quote in value of '{key}'"
            ))
        };
        let trailing = || {
            Error::General(format!(
                "{display_path}:{line_no}: unexpected text after closing quote in value of '{key}'"
            ))
        };

        let (raw, quoting) = if let Some(rest) = value_part.strip_prefix('\'') {
            // Single-quoted: no escapes, closing quote must be on this line.
            let Some(close) = rest.find('\'') else {
                return Err(unclosed());
            };
            if !rest[close + 1..].trim().is_empty() {
                return Err(trailing());
            }
            (rest[..close].to_string(), Quoting::Single)
        } else if let Some(rest) = value_part.strip_prefix('"') {
            // Double-quoted: only `\\` and `\"` are escape sequences; any
            // other backslash is kept verbatim.
            let mut raw = String::new();
            let mut end = None;
            let mut it = rest.char_indices();
            while let Some((pos, c)) = it.next() {
                match c {
                    '"' => {
                        end = Some(pos + 1);
                        break;
                    }
                    '\\' => match it.clone().next() {
                        Some((_, next @ ('\\' | '"'))) => {
                            raw.push(next);
                            it.next();
                        }
                        _ => raw.push('\\'),
                    },
                    other => raw.push(other),
                }
            }
            let Some(end) = end else {
                return Err(unclosed());
            };
            if !rest[end..].trim().is_empty() {
                return Err(trailing());
            }
            (raw, Quoting::Double)
        } else {
            (value_part.trim_end().to_string(), Quoting::None)
        };

        entries.push((
            key.to_string(),
            FileValue {
                raw,
                quoting,
                origin: format!("{display_path}:{line_no}"),
            },
        ));
    }

    Ok(entries)
}

/// 優先順位適用済みの最終マップを受け取り、File(None/Double) の値を展開して
/// 全キーの最終文字列マップを返す。
pub fn resolve(map: &BTreeMap<String, Entry>) -> Result<BTreeMap<String, String>> {
    let mut memo = BTreeMap::new();
    let mut visiting = Vec::new();
    for (name, entry) in map {
        resolve_entry(name, entry, map, &mut memo, &mut visiting)?;
    }
    // Every key in `map` has been resolved into the memo exactly once.
    Ok(memo)
}

/// Resolves one entry to its final string, memoizing the result and using
/// the `visiting` stack to detect circular references. Single-quoted file
/// values and literals are returned as-is; unquoted and double-quoted file
/// values are expanded against `map`.
fn resolve_entry(
    name: &str,
    entry: &Entry,
    map: &BTreeMap<String, Entry>,
    memo: &mut BTreeMap<String, String>,
    visiting: &mut Vec<String>,
) -> Result<String> {
    if let Some(value) = memo.get(name) {
        return Ok(value.clone());
    }

    let value = match entry {
        Entry::Literal(s) => s.clone(),
        Entry::File(fv) => match fv.quoting {
            Quoting::Single => fv.raw.clone(),
            Quoting::None | Quoting::Double => {
                if visiting.iter().any(|v| v == name) {
                    return Err(Error::General(format!(
                        "{}: circular variable reference involving '{name}'",
                        fv.origin
                    )));
                }
                visiting.push(name.to_string());
                let expanded = expand(&fv.raw, &fv.origin, name, map, memo, visiting);
                visiting.pop();
                expanded?
            }
        },
    };

    memo.insert(name.to_string(), value.clone());
    Ok(value)
}

fn invalid_ref(origin: &str, owner: &str) -> Error {
    Error::General(format!(
        "{origin}: invalid variable reference in value of '{owner}'"
    ))
}

fn unterminated(origin: &str, owner: &str) -> Error {
    Error::General(format!(
        "{origin}: unterminated '${{' in value of '{owner}'"
    ))
}

/// Expands `${NAME}` and `${NAME:-default}` references in `input`.
///
/// A `$` not followed by `{` is a literal `$` (the `$NAME` form is not
/// supported, and `$(`/backticks are plain characters). `${NAME}` requires
/// `NAME` to be defined in `map`; `${NAME:-default}` falls back to
/// `default` when `NAME` is undefined or resolves to the empty string.
/// Defaults may nest further `${...}` references (brace depth is tracked
/// to find the matching `}`) and are expanded under the same rules.
fn expand(
    input: &str,
    origin: &str,
    owner: &str,
    map: &BTreeMap<String, Entry>,
    memo: &mut BTreeMap<String, String>,
    visiting: &mut Vec<String>,
) -> Result<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '$' || i + 1 >= chars.len() || chars[i + 1] != '{' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 2; // consume "${"

        // Variable name: [A-Za-z_][A-Za-z0-9_]*.
        let name_start = i;
        if i < chars.len() && (chars[i].is_ascii_alphabetic() || chars[i] == '_') {
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
        }
        if i == name_start {
            if i >= chars.len() {
                return Err(unterminated(origin, owner));
            }
            return Err(invalid_ref(origin, owner));
        }
        let name: String = chars[name_start..i].iter().collect();

        if i >= chars.len() {
            return Err(unterminated(origin, owner));
        }
        if chars[i] == '}' {
            // ${NAME}: must be defined.
            i += 1;
            let Some(entry) = map.get(&name) else {
                return Err(Error::General(format!(
                    "{origin}: undefined variable '{name}' referenced by '{owner}'"
                )));
            };
            out.push_str(&resolve_entry(&name, entry, map, memo, visiting)?);
        } else if chars[i] == ':' && i + 1 < chars.len() && chars[i + 1] == '-' {
            // ${NAME:-default}: scan to the matching '}', tracking nested
            // "${" openers so defaults may contain further references.
            i += 2;
            let default_start = i;
            let mut depth = 0usize;
            let mut close = None;
            while i < chars.len() {
                if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if chars[i] == '}' {
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                    depth -= 1;
                }
                i += 1;
            }
            let Some(close) = close else {
                return Err(unterminated(origin, owner));
            };
            let default: String = chars[default_start..close].iter().collect();
            i = close + 1;

            let resolved = match map.get(&name) {
                None => None,
                Some(entry) => {
                    let value = resolve_entry(&name, entry, map, memo, visiting)?;
                    if value.is_empty() {
                        None
                    } else {
                        Some(value)
                    }
                }
            };
            match resolved {
                Some(value) => out.push_str(&value),
                None => out.push_str(&expand(&default, origin, owner, map, memo, visiting)?),
            }
        } else {
            // Anything else after the name (e.g. "${NAME:+x}") is invalid.
            return Err(invalid_ref(origin, owner));
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Vec<(String, FileValue)> {
        parse_env_str(source, "test.env").unwrap()
    }

    fn parse_err(source: &str) -> Error {
        parse_env_str(source, "test.env").unwrap_err()
    }

    fn file(raw: &str, quoting: Quoting) -> Entry {
        Entry::File(FileValue {
            raw: raw.to_string(),
            quoting,
            origin: "test.env:1".to_string(),
        })
    }

    fn map(entries: &[(&str, Entry)]) -> BTreeMap<String, Entry> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    // --- parsing ---

    #[test]
    fn parses_basic_key_value() {
        let entries = parse("FOO=bar\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "FOO");
        assert_eq!(entries[0].1.raw, "bar");
        assert_eq!(entries[0].1.quoting, Quoting::None);
        assert_eq!(entries[0].1.origin, "test.env:1");
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let entries = parse("# comment\n\n   \n  # indented comment\nFOO=bar\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "FOO");
        assert_eq!(entries[0].1.origin, "test.env:5");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let entries = parse("FOO=bar\r\nBAZ=qux\r\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1.raw, "bar");
        assert_eq!(entries[1].1.raw, "qux");
    }

    #[test]
    fn allows_whitespace_around_key_and_value() {
        let entries = parse("  FOO = bar  \n");
        assert_eq!(entries[0].0, "FOO");
        assert_eq!(entries[0].1.raw, "bar");
    }

    #[test]
    fn keeps_duplicate_keys_in_order() {
        let entries = parse("FOO=first\nFOO=second\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1.raw, "first");
        assert_eq!(entries[1].1.raw, "second");
    }

    #[test]
    fn unquoted_value_keeps_inline_comment_text() {
        let entries = parse("FOO=bar # not a comment\n");
        assert_eq!(entries[0].1.raw, "bar # not a comment");
    }

    #[test]
    fn unquoted_value_may_be_empty() {
        let entries = parse("FOO=\n");
        assert_eq!(entries[0].1.raw, "");
        assert_eq!(entries[0].1.quoting, Quoting::None);
    }

    #[test]
    fn single_quoted_value_is_raw() {
        let entries = parse("FOO='${BAR} \\n literal'\n");
        assert_eq!(entries[0].1.raw, "${BAR} \\n literal");
        assert_eq!(entries[0].1.quoting, Quoting::Single);
    }

    #[test]
    fn double_quoted_value_unescapes_backslash_and_quote() {
        let entries = parse(r#"FOO="a\\b\"c\nd""#);
        assert_eq!(entries[0].1.raw, "a\\b\"c\\nd");
        assert_eq!(entries[0].1.quoting, Quoting::Double);
    }

    #[test]
    fn unclosed_quote_is_error_without_value_content() {
        let err = parse_err("FOO='secret-value\n");
        let msg = err.to_string();
        assert_eq!(msg, "test.env:1: unclosed quote in value of 'FOO'");
        assert!(!msg.contains("secret-value"));

        let err = parse_err("FOO=\"secret-value\n");
        let msg = err.to_string();
        assert_eq!(msg, "test.env:1: unclosed quote in value of 'FOO'");
        assert!(!msg.contains("secret-value"));
    }

    #[test]
    fn text_after_closing_quote_is_error_without_value_content() {
        let err = parse_err("FOO='secret' trailing\n");
        let msg = err.to_string();
        assert_eq!(
            msg,
            "test.env:1: unexpected text after closing quote in value of 'FOO'"
        );
        assert!(!msg.contains("secret"));
    }

    #[test]
    fn invalid_line_is_error_without_line_content() {
        for source in ["no_equals_secret\n", "1BAD=x\n", "=x\n", "FO O=x\n"] {
            let msg = parse_err(source).to_string();
            assert_eq!(msg, "test.env:1: invalid env line (expected KEY=VALUE)");
            assert!(!msg.contains("secret"));
        }
    }

    #[test]
    fn parse_env_file_not_found() {
        let err = parse_env_file(Path::new("/nonexistent/portool-test.env")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "env file not found: /nonexistent/portool-test.env"
        );
    }

    // --- expansion ---

    #[test]
    fn expands_simple_reference() {
        let m = map(&[
            ("PORT", Entry::Literal("5432".to_string())),
            ("URL", file("localhost:${PORT}/db", Quoting::None)),
        ]);
        let resolved = resolve(&m).unwrap();
        assert_eq!(resolved["URL"], "localhost:5432/db");
        assert_eq!(resolved["PORT"], "5432");
    }

    #[test]
    fn default_used_when_undefined() {
        let m = map(&[("URL", file("${PORT:-5432}", Quoting::None))]);
        assert_eq!(resolve(&m).unwrap()["URL"], "5432");
    }

    #[test]
    fn default_used_when_empty() {
        let m = map(&[
            ("PORT", Entry::Literal(String::new())),
            ("URL", file("${PORT:-5432}", Quoting::None)),
        ]);
        assert_eq!(resolve(&m).unwrap()["URL"], "5432");
    }

    #[test]
    fn default_not_used_when_defined_and_non_empty() {
        let m = map(&[
            ("PORT", Entry::Literal("9999".to_string())),
            ("URL", file("${PORT:-5432}", Quoting::None)),
        ]);
        assert_eq!(resolve(&m).unwrap()["URL"], "9999");
    }

    #[test]
    fn nested_default_is_expanded() {
        let m = map(&[
            ("B", Entry::Literal("fallback".to_string())),
            ("X", file("${A:-${B}}", Quoting::None)),
        ]);
        assert_eq!(resolve(&m).unwrap()["X"], "fallback");
    }

    #[test]
    fn file_value_can_reference_file_value() {
        let m = map(&[
            ("BASE", file("localhost", Quoting::None)),
            ("URL", file("http://${BASE}/", Quoting::Double)),
        ]);
        assert_eq!(resolve(&m).unwrap()["URL"], "http://localhost/");
    }

    #[test]
    fn file_value_can_reference_literal() {
        let m = map(&[
            ("HOME_DIR", Entry::Literal("/home/user".to_string())),
            ("CACHE", file("${HOME_DIR}/.cache", Quoting::None)),
        ]);
        assert_eq!(resolve(&m).unwrap()["CACHE"], "/home/user/.cache");
    }

    #[test]
    fn undefined_reference_is_error() {
        let m = map(&[("URL", file("${MISSING}", Quoting::None))]);
        assert_eq!(
            resolve(&m).unwrap_err().to_string(),
            "test.env:1: undefined variable 'MISSING' referenced by 'URL'"
        );
    }

    #[test]
    fn circular_reference_is_error() {
        let m = map(&[
            ("A", file("${B}", Quoting::None)),
            ("B", file("${A}", Quoting::None)),
        ]);
        let msg = resolve(&m).unwrap_err().to_string();
        assert!(
            msg.contains("circular variable reference involving"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn self_reference_is_error() {
        let m = map(&[("A", file("${A}", Quoting::None))]);
        assert_eq!(
            resolve(&m).unwrap_err().to_string(),
            "test.env:1: circular variable reference involving 'A'"
        );
    }

    #[test]
    fn dollar_without_brace_is_literal() {
        let m = map(&[(
            "X",
            file("$NAME and $(cmd) and `tick` and $", Quoting::None),
        )]);
        assert_eq!(
            resolve(&m).unwrap()["X"],
            "$NAME and $(cmd) and `tick` and $"
        );
    }

    #[test]
    fn single_quoted_is_not_expanded() {
        let m = map(&[
            ("PORT", Entry::Literal("5432".to_string())),
            ("X", file("${PORT}", Quoting::Single)),
        ]);
        assert_eq!(resolve(&m).unwrap()["X"], "${PORT}");
    }

    #[test]
    fn literal_is_not_expanded() {
        let m = map(&[("X", Entry::Literal("${NOT_EXPANDED}".to_string()))]);
        assert_eq!(resolve(&m).unwrap()["X"], "${NOT_EXPANDED}");
    }

    #[test]
    fn invalid_variable_name_is_error() {
        let m = map(&[("X", file("${1BAD}", Quoting::None))]);
        assert_eq!(
            resolve(&m).unwrap_err().to_string(),
            "test.env:1: invalid variable reference in value of 'X'"
        );
    }

    #[test]
    fn unterminated_reference_is_error() {
        let m = map(&[("X", file("${NAME", Quoting::None))]);
        assert_eq!(
            resolve(&m).unwrap_err().to_string(),
            "test.env:1: unterminated '${' in value of 'X'"
        );
    }
}
