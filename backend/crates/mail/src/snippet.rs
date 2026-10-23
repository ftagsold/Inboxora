//! Message preview snippets (port of `messageParser.ts` snippet builders).
//!
//! The Node implementation is a chain of regexes, several with lookbehinds and
//! backreferences. Each is reproduced here as a linear scan with the same result, so crafted
//! bodies cannot cause super-linear work.

use std::sync::LazyLock;

use regex::Regex;

use crate::html::{self, Event};
use crate::text::{collapse_ws, drop_tokens, is_js_space, js_len, js_substring};

/// Invisible / zero-width "preheader killer" characters.
pub fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{200B}'
            | '\u{200C}'
            | '\u{200D}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{2007}'
            | '\u{2060}'
            | '\u{2061}'
            | '\u{2062}'
            | '\u{2063}'
            | '\u{2064}'
            | '\u{FEFF}'
    )
}

pub fn strip_invisible(s: &str) -> String {
    s.chars().filter(|c| !is_invisible(*c)).collect()
}

/// Decoded value of the named entities common in mail bodies; unknown names become a space.
pub fn decode_named_entity(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "hellip" | "mldr" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "minus" => "−",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "bull" => "•",
        "middot" => "·",
        "laquo" => "«",
        "raquo" => "»",
        "lsaquo" => "‹",
        "rsaquo" => "›",
        "trade" => "™",
        "reg" => "®",
        "copy" => "©",
        "deg" => "°",
        "micro" => "µ",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "times" => "×",
        "divide" => "÷",
        "plusmn" => "±",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "rarr" => "→",
        "larr" => "←",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "thinsp" | "ensp" | "emsp" | "hairsp" | "nnbsp" => " ",
        "shy" | "zwnj" | "zwj" | "lrm" | "rlm" => "",
        _ => " ",
    }
}

fn looks_like_html(text: &str) -> bool {
    static DOC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^\s*<(?:!doctype|html|head|body)[\s>]").expect("regex"));
    static ATTR_TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)<[a-z][a-z0-9]*\s[^>]*=[^>]*>").expect("regex"));
    static STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<(?:style|script)[\s>]").expect("regex"));
    static CLOSING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</[a-z][a-z0-9]*\s*>").expect("regex"));
    let head = js_substring(text, 2048);
    DOC.is_match(&head) || ATTR_TAG.is_match(&head) || STYLE.is_match(&head) || CLOSING.find_iter(&head).count() >= 3
}

fn is_degenerate_text(text: &str) -> bool {
    static CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</[a-z][a-z0-9:-]*\s*>").expect("regex"));
    static EMPTY_WRAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:\(\s*\)|\[\s*\]|<\s*>)").expect("regex"));
    text.contains("<!--") || CLOSE.is_match(text) || EMPTY_WRAP.is_match(text)
}

/// `(?<=^|[\s{}<>])[^\s{}<>][^{}<>]{0,160}\{(?=[^{}]*[:;])[^{}]*\}` → `' '` (CSS blocks in
/// text bodies), implemented as a forward scan.
fn strip_css_blocks(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let boundary = |c: char| is_js_space(c) || matches!(c, '{' | '}' | '<' | '>');
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || boundary(chars[i - 1]);
        if at_boundary && !boundary(chars[i]) {
            // Selector: up to 160 more chars of [^{}<>], then '{'.
            let mut j = i + 1;
            let mut count = 0;
            let mut matched = None;
            loop {
                if j < chars.len() && chars[j] == '{' {
                    // Body [^{}]* then '}', containing a ':' or ';'.
                    let mut k = j + 1;
                    let mut has_sep = false;
                    while k < chars.len() && chars[k] != '{' && chars[k] != '}' {
                        if chars[k] == ':' || chars[k] == ';' {
                            has_sep = true;
                        }
                        k += 1;
                    }
                    if has_sep && k < chars.len() && chars[k] == '}' {
                        matched = Some(k + 1);
                    }
                    // The regex would backtrack to a shorter selector only if '{' could be
                    // consumed by [^{}<>], which it cannot.
                    break;
                }
                if j >= chars.len() || count >= 160 || matches!(chars[j], '{' | '}' | '<' | '>') {
                    break;
                }
                j += 1;
                count += 1;
            }
            if let Some(end) = matched {
                out.push(' ');
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `(?<![\w@.])www\.[^\s<>()[\]]+` → `''`.
fn strip_www(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let stop = |c: char| is_js_space(c) || matches!(c, '<' | '>' | '(' | ')' | '[' | ']');
    while i < chars.len() {
        let prev_ok = i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || matches!(chars[i - 1], '_' | '@' | '.'));
        let is_www = i + 4 < chars.len()
            && chars[i..i + 4].iter().map(|c| c.to_ascii_lowercase()).eq("www.".chars())
            && !stop(chars[i + 4]);
        if prev_ok && is_www {
            let mut j = i + 4;
            while j < chars.len() && !stop(chars[j]) {
                j += 1;
            }
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Decode `&#x..;`, `&#..;` and named entities (unknown → space).
fn decode_entities(s: &str) -> String {
    static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#x([0-9A-Fa-f]+);").expect("regex"));
    static DEC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#([0-9]+);").expect("regex"));
    static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&([a-z][a-z0-9]*);").expect("regex"));
    // Out-of-range code points would make String.fromCodePoint throw; use U+FFFD instead.
    let cp = |v: Option<u32>| v.and_then(char::from_u32).unwrap_or('\u{FFFD}').to_string();
    let s = HEX.replace_all(s, |c: &regex::Captures| cp(u32::from_str_radix(&c[1], 16).ok()));
    let s = DEC.replace_all(&s, |c: &regex::Captures| cp(c[1].parse::<u32>().ok()));
    NAMED.replace_all(&s, |c: &regex::Captures| decode_named_entity(&c[1]).to_owned()).into_owned()
}

/// `(?<=^|\s)([=_*#~-])\1{3,}(?=\s|$)` → `''`.
fn is_divider_run(tok: &str) -> bool {
    let mut chars = tok.chars();
    let Some(first) = chars.next() else { return false };
    matches!(first, '=' | '_' | '*' | '#' | '~' | '-') && tok.chars().count() >= 4 && tok.chars().all(|c| c == first)
}

/// `(?<=^|\s)[*_]{2,}(?=\s|$)` → `''`.
fn is_emphasis_run(tok: &str) -> bool {
    tok.chars().count() >= 2 && tok.chars().all(|c| c == '*' || c == '_')
}

/// Build a plain-text snippet from a decoded text/plain body and/or a text/html body.
pub fn snippet_from_body(text: &str, html: Option<&str>) -> String {
    static IMAGE_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\[image:[^\]]*\]").expect("regex"));
    static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<!--[\s\S]*?(?:-->|$)").expect("regex"));
    static CLOSE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</[a-z][a-z0-9:-]*\s*>").expect("regex"));
    static MD_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]\r\n]*)\]\([^)\r\n]*\)").expect("regex"));
    static URL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:https?://|mailto:)[^\s<>()\[\]]+").expect("regex"));
    static EMPTY_WRAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(\s*\)|\[\s*\]|<\s*>").expect("regex"));

    let mut text = text.to_owned();
    if !text.is_empty() && looks_like_html(&text) {
        let stripped = build_snippet_from_html(&text);
        if !stripped.is_empty() {
            return stripped;
        }
        text.clear();
    }
    if let Some(h) = html.filter(|h| !h.is_empty())
        && !text.is_empty()
        && is_degenerate_text(&text)
    {
        let stripped = build_snippet_from_html(h);
        if !stripped.is_empty() {
            return stripped;
        }
    }
    if !text.is_empty() {
        let mut cleaned = IMAGE_PLACEHOLDER.replace_all(&text, "").into_owned();
        if cleaned.contains('{') {
            cleaned = strip_css_blocks(&cleaned);
        }
        let cleaned = COMMENT.replace_all(&cleaned, " ");
        let cleaned = CLOSE_TAG.replace_all(&cleaned, " ");
        let cleaned = MD_LINK.replace_all(&cleaned, "$1");
        let cleaned = URL.replace_all(&cleaned, "");
        let cleaned = strip_www(&cleaned);
        let cleaned = decode_entities(&cleaned);
        let cleaned = strip_invisible(&cleaned);
        let cleaned = EMPTY_WRAP.replace_all(&cleaned, " ");
        let cleaned = drop_tokens(&cleaned, is_emphasis_run);
        let cleaned = collapse_ws(&cleaned);
        let cleaned = drop_tokens(&cleaned, is_divider_run);
        let cleaned = collapse_ws(&cleaned);
        if !cleaned.is_empty() {
            return js_substring(&cleaned, 200);
        }
    }
    match html {
        Some(h) if !h.is_empty() => build_snippet_from_html(h),
        _ => String::new(),
    }
}

const SNIPPET_VISIBLE_CAP: usize = 260;

fn extract_html_snippet_text(html_in: &str) -> String {
    let mut out = String::new();
    let mut skip_depth = 0u32;
    let mut visible = 0usize;
    let mut done = false;
    let skip_tag = |n: &str| matches!(n, "script" | "style" | "head" | "title" | "noscript");
    let add_sep = |out: &mut String| {
        if !out.is_empty() && !out.chars().last().is_some_and(is_js_space) {
            out.push(' ');
        }
    };
    html::parse(html_in, |ev| match ev {
        Event::Open { name, .. } => {
            if skip_tag(&name) {
                skip_depth += 1;
            } else if skip_depth == 0 {
                add_sep(&mut out);
            }
        }
        Event::Close { name, .. } => {
            if skip_tag(&name) {
                skip_depth = skip_depth.saturating_sub(1);
            } else if skip_depth == 0 {
                add_sep(&mut out);
            }
        }
        Event::Text(t) => {
            if skip_depth > 0 || done {
                return;
            }
            let clean = strip_invisible(&t);
            if clean.is_empty() {
                return;
            }
            if clean.chars().all(is_js_space) {
                add_sep(&mut out);
                return;
            }
            visible += js_len(&clean.chars().filter(|c| !is_js_space(*c)).collect::<String>());
            out.push_str(&clean);
            if visible >= SNIPPET_VISIBLE_CAP {
                done = true;
            }
        }
    });
    out
}

/// Plain-text snippet from an HTML body.
pub fn build_snippet_from_html(html_in: &str) -> String {
    static MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"##[^#]*##").expect("regex"));
    let text = extract_html_snippet_text(html_in);
    let text = MARKER.replace_all(&text, "");
    let text = strip_invisible(&text);
    let text = drop_tokens(&text, is_divider_run);
    js_substring(&collapse_ws(&text), 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_blocks_need_a_declaration_separator() {
        assert_eq!(strip_css_blocks("a .x{color:red} b"), "  b");
        assert_eq!(strip_css_blocks("Hi\n.x{color:red} b"), "  b");
        assert_eq!(strip_css_blocks(&format!("{} .x{{a:b}}", "y".repeat(200))), format!("{}  ", "y".repeat(200)));
        assert_eq!(strip_css_blocks("set {a, b} here"), "set {a, b} here");
    }

    #[test]
    fn www_requires_a_boundary() {
        assert_eq!(strip_www("see www.example.com now"), "see  now");
        assert_eq!(strip_www("me@www.example.com"), "me@www.example.com");
    }

    #[test]
    fn text_snippets_clean_links_and_entities() {
        assert_eq!(
            snippet_from_body(
                "[image: Banner] Hello [Shop now](https://x.test/a) &hellip; see https://x.test ( ) ==== end",
                None
            ),
            "Hello Shop now … see end"
        );
    }
}
