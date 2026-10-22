//! JavaScript string semantics the ported code depends on.

use serde_json::Value;

/// JS `\s`: ECMAScript WhiteSpace + LineTerminator.
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `s.replace(/\s+/g, ' ').trim()`.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if is_js_space(c) {
            in_space = true;
        } else {
            if in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = false;
            out.push(c);
        }
    }
    out
}

/// `s.trim()` with JS whitespace.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `s.substring(0, n)` counted in UTF-16 code units. A surrogate pair cut in half leaves a
/// lone surrogate in JS, which becomes U+FFFD when encoded as UTF-8.
pub fn js_substring(s: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        let len = c.len_utf16();
        if units + len > n {
            if units < n {
                out.push('\u{FFFD}');
            }
            break;
        }
        units += len;
        out.push(c);
    }
    out
}

/// UTF-16 length (`s.length`).
pub fn js_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `String(v)` for a JSON value.
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map_or_else(
            || n.to_string(),
            |f| if f.fract() == 0.0 && f.abs() < 1e21 { format!("{}", f as i64) } else { n.to_string() },
        ),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            items.iter().map(|i| if i.is_null() { String::new() } else { js_string(i) }).collect::<Vec<_>>().join(",")
        }
        Value::Object(_) => "[object Object]".into(),
    }
}

/// Tokens separated by JS whitespace, keeping the separators (for whitespace-bounded rules).
pub fn split_keep_ws(s: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut cur_space: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let sp = is_js_space(c);
        match cur_space {
            Some(prev) if prev != sp => {
                out.push((prev, &s[start..i]));
                start = i;
                cur_space = Some(sp);
            }
            None => cur_space = Some(sp),
            _ => {}
        }
    }
    if let Some(sp) = cur_space {
        out.push((sp, &s[start..]));
    }
    out
}

/// Remove whitespace-bounded tokens for which `drop` returns true (the separators stay).
pub fn drop_tokens(s: &str, drop: impl Fn(&str) -> bool) -> String {
    split_keep_ws(s).into_iter().filter(|(space, tok)| *space || !drop(tok)).map(|(_, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_counts_utf16() {
        assert_eq!(js_substring("a😀b", 2), "a\u{FFFD}");
        assert_eq!(js_substring("a😀b", 3), "a😀");
        assert_eq!(collapse_ws("  a \u{A0}\n b  "), "a b");
    }
}
