//! Inline `style` attribute normalisation as sanitize-html does it with postcss:
//! parse `tag {value}` and re-serialise the first rule's declarations as
//! `prop:value[ !important]` joined with `;`. Unparseable input (a declaration without a
//! colon, an unclosed string or bracket) rejects the whole attribute.

/// Normalise a style attribute value. `None` means "drop the attribute".
pub fn normalize_style(value: &str) -> Option<String> {
    let decls = parse_declarations(value)?;
    let out: Vec<String> = decls
        .into_iter()
        .map(|d| format!("{}:{}{}", d.prop, d.value, if d.important { " !important" } else { "" }))
        .collect();
    let joined = out.join(";");
    (!joined.is_empty()).then_some(joined)
}

#[derive(Debug, PartialEq)]
struct Decl {
    prop: String,
    value: String,
    important: bool,
}

/// Split the block body into raw declaration segments, honouring strings, parentheses,
/// escapes and comments. Stops at a top-level `}` (postcss closes the rule there and later
/// nodes are not read by sanitize-html). Comments are removed.
fn segments(input: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '"' | '\'' => {
                cur.push(c);
                let mut closed = false;
                while let Some(n) = chars.next() {
                    cur.push(n);
                    if n == '\\' {
                        if let Some(e) = chars.next() {
                            cur.push(e);
                        }
                        continue;
                    }
                    if n == c {
                        closed = true;
                        break;
                    }
                    if n == '\n' {
                        break;
                    }
                }
                if !closed {
                    return None; // Unclosed string
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                cur.push(' ');
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ';' if depth == 0 => out.push(std::mem::take(&mut cur)),
            '}' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                return Some(out);
            }
            '{' if depth == 0 => return None,
            _ => cur.push(c),
        }
    }
    if depth > 0 {
        return None; // Unclosed bracket
    }
    out.push(cur);
    Some(out)
}

fn parse_declarations(input: &str) -> Option<Vec<Decl>> {
    let mut decls = Vec::new();
    for seg in segments(input)? {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let colon = top_level_colon(seg)?;
        let prop = seg[..colon].trim();
        if prop.is_empty() || prop.contains(char::is_whitespace) {
            return None; // Unknown word
        }
        let mut value = seg[colon + 1..].trim().to_owned();
        let mut important = false;
        if let Some(idx) = find_important(&value) {
            important = true;
            value = value[..idx].trim_end().to_owned();
        }
        decls.push(Decl { prop: prop.to_owned(), value, important });
    }
    Some(decls)
}

fn top_level_colon(seg: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in seg.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (_, '\\') => escaped = true,
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ':') if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Position of a trailing `!important` (also `! important`), case-insensitive.
fn find_important(value: &str) -> Option<usize> {
    let lower = value.to_ascii_lowercase();
    let trimmed = lower.trim_end();
    let word = trimmed.strip_suffix("important")?;
    let before = word.trim_end();
    let bang = before.strip_suffix('!')?;
    Some(bang.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_sanitize_html_output() {
        assert_eq!(
            normalize_style("color:red;;font-weight: bold !important; background: url(http://x.test/bg.png)")
                .as_deref(),
            Some("color:red;font-weight:bold !important;background:url(http://x.test/bg.png)")
        );
        assert_eq!(normalize_style("  "), None);
        assert_eq!(normalize_style("color red"), None);
        assert_eq!(
            normalize_style("font-family: 'Helvetica Neue', Arial; margin: 0 auto").as_deref(),
            Some("font-family:'Helvetica Neue', Arial;margin:0 auto")
        );
        assert_eq!(normalize_style("a:b}c{d:e").as_deref(), Some("a:b"));
        assert_eq!(normalize_style("color:RED;Font-Size:12PX").as_deref(), Some("color:RED;Font-Size:12PX"));
        assert_eq!(normalize_style("content: \"a;b\"").as_deref(), Some("content:\"a;b\""));
        assert_eq!(
            normalize_style("background:url(data:image/png;base64,AA==)").as_deref(),
            Some("background:url(data:image/png;base64,AA==)")
        );
        assert_eq!(normalize_style("color: 'unclosed"), None);
        assert_eq!(normalize_style("width: calc(1px"), None);
    }
}
