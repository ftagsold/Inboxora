//! HTML sanitisation (port of `emailSanitizer.ts` and the parts of `sanitize-html` 2.18 it
//! uses, in `discard` mode).
//!
//! Output is built only from parser events: allowed tags are re-serialised with filtered,
//! re-escaped attributes; text is re-escaped; only `<style>`/`<script>` content of an
//! allowed element is copied verbatim (the tokenizer guarantees it cannot contain the
//! element's end tag). Comments, doctypes and disallowed tags never reach the output.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::html::{self, Event};

type Transform = fn(&str, &mut Vec<(String, String)>);

/// A sanitize-html policy (only the options the Node code uses).
pub struct Policy {
    allowed_tags: &'static [&'static str],
    /// `(tag, attributes)`; tag `*` applies to every tag.
    allowed_attrs: &'static [(&'static str, &'static [&'static str])],
    allowed_schemes: &'static [&'static str],
    schemes_by_tag: &'static [(&'static str, &'static [&'static str])],
    /// Tag-specific transforms run first, then the `*` transform (sanitize-html order).
    transforms: &'static [(&'static str, Transform)],
}

const NON_TEXT_TAGS: [&str; 5] = ["script", "style", "textarea", "option", "xmp"];
const SELF_CLOSING: [&str; 10] = ["img", "br", "hr", "area", "base", "basefont", "input", "link", "meta", "col"];
const SCHEME_CHECKED_ATTRS: [&str; 20] = [
    "href",
    "src",
    "cite",
    "action",
    "formaction",
    "data",
    "xlink:href",
    "poster",
    "background",
    "ping",
    "longdesc",
    "usemap",
    "codebase",
    "classid",
    "archive",
    "profile",
    "manifest",
    "itemid",
    "dynsrc",
    "lowsrc",
];

/// sanitize-html's `nonBooleanAttributes`: an empty value removes these.
fn is_non_boolean(a: &str) -> bool {
    matches!(
        a,
        "abbr"
            | "accept"
            | "accept-charset"
            | "accesskey"
            | "action"
            | "allow"
            | "alt"
            | "as"
            | "autocapitalize"
            | "autocomplete"
            | "blocking"
            | "charset"
            | "cite"
            | "class"
            | "color"
            | "cols"
            | "colspan"
            | "content"
            | "contenteditable"
            | "coords"
            | "crossorigin"
            | "data"
            | "datetime"
            | "decoding"
            | "dir"
            | "dirname"
            | "download"
            | "draggable"
            | "enctype"
            | "enterkeyhint"
            | "fetchpriority"
            | "for"
            | "form"
            | "formaction"
            | "formenctype"
            | "formmethod"
            | "formtarget"
            | "headers"
            | "height"
            | "hidden"
            | "high"
            | "href"
            | "hreflang"
            | "http-equiv"
            | "id"
            | "imagesizes"
            | "imagesrcset"
            | "inputmode"
            | "integrity"
            | "is"
            | "itemid"
            | "itemprop"
            | "itemref"
            | "itemtype"
            | "kind"
            | "label"
            | "lang"
            | "list"
            | "loading"
            | "low"
            | "max"
            | "maxlength"
            | "media"
            | "method"
            | "min"
            | "minlength"
            | "name"
            | "nonce"
            | "optimum"
            | "pattern"
            | "ping"
            | "placeholder"
            | "popover"
            | "popovertarget"
            | "popovertargetaction"
            | "poster"
            | "preload"
            | "referrerpolicy"
            | "rel"
            | "rows"
            | "rowspan"
            | "sandbox"
            | "scope"
            | "shape"
            | "size"
            | "sizes"
            | "slot"
            | "span"
            | "spellcheck"
            | "src"
            | "srcdoc"
            | "srclang"
            | "srcset"
            | "start"
            | "step"
            | "style"
            | "tabindex"
            | "target"
            | "title"
            | "translate"
            | "type"
            | "usemap"
            | "value"
            | "width"
            | "wrap"
    ) || (a.starts_with("on") && a.len() > 2)
}

fn valid_attr_name(a: &str) -> bool {
    !a.is_empty() && !a.chars().any(|c| matches!(c, '\0' | '\t' | '\n' | '\x0c' | '\r' | ' ' | '/' | '<' | '=' | '>'))
}

/// `escapeHtml` with `decodeEntities: true`.
pub fn escape_html(s: &str, quote: bool) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// launder's `cleanHref`: drop control/space characters and HTML comments.
fn clean_href(href: &str) -> String {
    let mut s: String = href.chars().filter(|c| *c > '\x20').collect();
    while let Some(first) = s.find("<!--") {
        let Some(rel) = s[first + 4..].find("-->") else { break };
        let last = first + 4 + rel;
        s = format!("{}{}", &s[..first], &s[last + 3..]);
    }
    s
}

/// launder's `naughtyHref` with sanitize-html's per-tag schemes (protocol-relative allowed).
fn naughty_href(policy: &Policy, tag: &str, href: &str) -> bool {
    let schemes = policy.schemes_by_tag.iter().find(|(t, _)| *t == tag).map_or(policy.allowed_schemes, |(_, s)| *s);
    let h = clean_href(href);
    let bytes = h.as_bytes();
    if bytes.first().is_some_and(u8::is_ascii_alphabetic) {
        let end = bytes.iter().position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')));
        if let Some(i) = end
            && bytes[i] == b':'
        {
            let scheme = h[..i].to_ascii_lowercase();
            return !schemes.contains(&scheme.as_str());
        }
    }
    false
}

struct Frame {
    tag: String,
}

/// Run a policy over `html` (sanitize-html `discard` mode).
pub fn sanitize(html: &str, policy: &Policy) -> String {
    sanitize_at(html, policy, 0)
}

fn sanitize_at(input: &str, policy: &Policy, iframe_depth: u32) -> String {
    let tag_allowed = |t: &str| policy.allowed_tags.contains(&t);
    let attr_allowed = |tag: &str, a: &str| {
        policy.allowed_attrs.iter().any(|(t, attrs)| (*t == tag || *t == "*") && attrs.contains(&a))
    };
    let mut result = String::with_capacity(input.len());
    let mut stack: Vec<Frame> = Vec::new();
    let mut skip_map: Vec<bool> = Vec::new(); // indexed by depth
    let mut skip_text = false;
    let mut skip_text_depth = 0u32;

    html::parse(input, |ev| match ev {
        Event::Open { name, attrs } => {
            if skip_text {
                skip_text_depth += 1;
                return;
            }
            stack.push(Frame { tag: name.clone() });
            let mut attrs = attrs;
            for (t, f) in policy.transforms {
                if *t == name {
                    f(&name, &mut attrs);
                }
            }
            for (t, f) in policy.transforms {
                if *t == "*" {
                    f(&name, &mut attrs);
                }
            }
            let skip = !tag_allowed(&name);
            let depth = stack.len() - 1;
            if skip_map.len() <= depth {
                skip_map.resize(depth + 1, false);
            }
            skip_map[depth] = skip;
            if skip {
                if NON_TEXT_TAGS.contains(&name.as_str()) {
                    skip_text = true;
                    skip_text_depth = 1;
                }
                return;
            }
            result.push('<');
            result.push_str(&name);
            for (a, value) in attrs {
                if !valid_attr_name(&a) {
                    continue;
                }
                if value.is_empty() && a != "alt" && is_non_boolean(&a) {
                    continue;
                }
                if !attr_allowed(&name, &a) {
                    continue;
                }
                let mut value = value;
                if SCHEME_CHECKED_ATTRS.contains(&a.as_str()) && naughty_href(policy, &name, &value) {
                    continue;
                }
                if a == "srcset" {
                    let kept: Vec<_> = crate::srcset::parse(&value)
                        .into_iter()
                        .filter(|c| !naughty_href(policy, &name, &c.url))
                        .collect();
                    if kept.is_empty() {
                        continue;
                    }
                    value = crate::srcset::stringify(&kept);
                }
                if a == "style" {
                    match crate::css::normalize_style(&value) {
                        Some(v) => value = v,
                        None => continue,
                    }
                }
                result.push(' ');
                result.push_str(&a);
                if !value.is_empty() {
                    result.push_str("=\"");
                    result.push_str(&escape_html(&value, true));
                    result.push('"');
                } else if a == "alt" {
                    result.push_str("=\"\"");
                }
            }
            if SELF_CLOSING.contains(&name.as_str()) {
                result.push_str(" />");
            } else {
                result.push('>');
            }
        }
        Event::Text(text) => {
            if skip_text {
                return;
            }
            let tag = stack.last().map(|f| f.tag.as_str());
            match tag {
                Some(t @ ("script" | "style")) if tag_allowed(t) => result.push_str(&text),
                Some("iframe") if !tag_allowed("iframe") && iframe_depth < 8 => {
                    // htmlparser2 delivers iframe fallback markup as text; re-sanitize it as HTML.
                    result.push_str(&sanitize_at(&text, policy, iframe_depth + 1));
                }
                _ => result.push_str(&escape_html(&text, false)),
            }
        }
        Event::Close { name, .. } => {
            if skip_text {
                skip_text_depth -= 1;
                if skip_text_depth > 0 {
                    return;
                }
                skip_text = false;
            }
            let Some(frame) = stack.pop() else { return };
            if frame.tag != name {
                stack.push(frame);
                return;
            }
            skip_text = false;
            let depth = stack.len();
            if skip_map.get(depth).copied().unwrap_or(false) {
                skip_map[depth] = false;
                return;
            }
            if SELF_CLOSING.contains(&name.as_str()) {
                return;
            }
            result.push_str("</");
            result.push_str(&name);
            result.push('>');
        }
    });
    result
}

// ── Email policy helpers ─────────────────────────────────────────────────────────────

fn upgrade_url(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_owned(),
    }
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Normalise an anchor href to an absolute https/mailto/tel URL, or `None` when it cannot be
/// resolved safely inside the same-origin srcdoc iframe (relative paths, fragments, …).
pub fn normalize_href(href: &str) -> Option<String> {
    let h = href.trim();
    if h.is_empty() {
        return None;
    }
    if starts_with_ci(h, "https://") {
        return Some(h.to_owned());
    }
    if starts_with_ci(h, "http://") {
        return Some(format!("https://{}", &h[7..]));
    }
    if ["mailto:", "cid:", "tel:", "sms:"].iter().any(|p| starts_with_ci(h, p)) {
        return Some(h.to_owned());
    }
    if let Some(rest) = h.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    if h.starts_with(['#', '/', '?', '.']) {
        return None;
    }
    if ["javascript:", "data:", "vbscript:"].iter().any(|p| starts_with_ci(h, p)) {
        return None;
    }
    if h.as_bytes()[0].is_ascii_alphanumeric() && h.contains('.') {
        return Some(format!("https://{h}"));
    }
    None
}

/// Query parameter lookup with URLSearchParams decoding.
fn query_param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

fn ebay_direct(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    if u.host_str() == Some("svcs.ebay.com") && u.path().starts_with("/imageser/") {
        let direct = query_param(&u, "imageUrl")?;
        if direct.starts_with("https://") {
            return Some(direct);
        }
    }
    None
}

fn unwrap_ebay_img_url(url: &str) -> String {
    if !url.contains("svcs.ebay.com/imageser") {
        return url.to_owned();
    }
    ebay_direct(url).unwrap_or_else(|| url.to_owned())
}

static STYLE_HTTP_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)url\(\s*(['"]?)http://"#).expect("regex"));

fn upgrade_style_urls(style: &str) -> String {
    STYLE_HTTP_URL.replace_all(style, |c: &Captures| format!("url({}https://", &c[1])).into_owned()
}

fn set_attr(attrs: &mut Vec<(String, String)>, key: &str, value: String) {
    match attrs.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => attrs.push((key.to_owned(), value)),
    }
}

fn get_attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn email_a(_: &str, attrs: &mut Vec<(String, String)>) {
    set_attr(attrs, "rel", "noopener noreferrer".into());
    if let Some(i) = attrs.iter().position(|(k, _)| k == "href") {
        match normalize_href(&attrs[i].1) {
            Some(n) => attrs[i].1 = n,
            None => {
                attrs.remove(i);
            }
        }
    }
}

static SRCSET_HTTP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bhttp://").expect("regex"));

fn email_img(_: &str, attrs: &mut Vec<(String, String)>) {
    if let Some(src) = get_attr(attrs, "src").filter(|s| !s.is_empty()).map(str::to_owned) {
        set_attr(attrs, "src", unwrap_ebay_img_url(&upgrade_url(&src)));
    }
    if let Some(v) = get_attr(attrs, "srcset").filter(|s| !s.is_empty()).map(str::to_owned) {
        set_attr(attrs, "srcset", SRCSET_HTTP.replace_all(&v, "https://").into_owned());
    }
    if let Some(v) = get_attr(attrs, "background").filter(|s| !s.is_empty()).map(str::to_owned) {
        set_attr(attrs, "background", upgrade_url(&v));
    }
    if let Some(v) = get_attr(attrs, "style").filter(|s| !s.is_empty()).map(str::to_owned) {
        set_attr(attrs, "style", upgrade_style_urls(&v));
    }
    let is_local = get_attr(attrs, "src").is_some_and(|s| starts_with_ci(s, "cid:") || starts_with_ci(s, "data:"));
    if !is_local {
        set_attr(attrs, "loading", "lazy".into());
    }
    set_attr(attrs, "decoding", "async".into());
}

fn email_any(_: &str, attrs: &mut Vec<(String, String)>) {
    if let Some(v) = get_attr(attrs, "background").filter(|s| !s.is_empty()).map(str::to_owned) {
        set_attr(attrs, "background", upgrade_url(&v));
    }
    if let Some(v) = get_attr(attrs, "style").filter(|s| !s.is_empty()).map(str::to_owned) {
        set_attr(attrs, "style", upgrade_style_urls(&v));
    }
}

fn compose_a(_: &str, attrs: &mut Vec<(String, String)>) {
    set_attr(attrs, "rel", "noopener noreferrer".into());
    set_attr(attrs, "target", "_blank".into());
}

static EMAIL_POLICY: Policy = Policy {
    allowed_tags: &[
        "div",
        "span",
        "p",
        "br",
        "hr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "table",
        "thead",
        "tbody",
        "tfoot",
        "tr",
        "th",
        "td",
        "caption",
        "colgroup",
        "col",
        "a",
        "img",
        "figure",
        "figcaption",
        "strong",
        "b",
        "em",
        "i",
        "u",
        "s",
        "del",
        "ins",
        "sub",
        "sup",
        "small",
        "big",
        "blockquote",
        "pre",
        "code",
        "tt",
        "kbd",
        "samp",
        "center",
        "font",
        "strike",
        "style",
    ],
    allowed_attrs: &[
        (
            "*",
            &[
                "style",
                "class",
                "id",
                "align",
                "valign",
                "width",
                "height",
                "bgcolor",
                "color",
                "border",
                "cellpadding",
                "cellspacing",
                "colspan",
                "rowspan",
                "nowrap",
                "dir",
                "lang",
                "background",
            ],
        ),
        ("a", &["href", "name", "target", "title", "rel"]),
        ("img", &["src", "alt", "width", "height", "border", "srcset", "sizes", "loading", "decoding"]),
        ("table", &["summary"]),
        ("td", &["abbr", "axis", "headers", "scope"]),
        ("th", &["abbr", "axis", "headers", "scope"]),
    ],
    allowed_schemes: &["http", "https", "mailto", "cid"],
    schemes_by_tag: &[("img", &["http", "https", "cid", "data"])],
    transforms: &[("a", email_a), ("img", email_img), ("*", email_any)],
};

static COMPOSE_POLICY: Policy = Policy {
    allowed_tags: &[
        "a",
        "b",
        "strong",
        "i",
        "em",
        "u",
        "s",
        "del",
        "p",
        "br",
        "div",
        "span",
        "img",
        "ul",
        "ol",
        "li",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "blockquote",
        "pre",
        "code",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
        "font",
        "center",
    ],
    allowed_attrs: &[
        ("*", &["style", "class"]),
        ("a", &["href", "target", "rel"]),
        ("img", &["src", "alt", "width", "height", "title"]),
        ("font", &["color", "size", "face"]),
        ("td", &["colspan", "rowspan", "align", "valign", "width", "height", "bgcolor"]),
        ("th", &["colspan", "rowspan", "align", "valign"]),
        ("table", &["width", "cellpadding", "cellspacing", "border", "align", "bgcolor"]),
        ("code", &["class"]),
    ],
    allowed_schemes: &["https", "mailto"],
    schemes_by_tag: &[("img", &["https", "data"])],
    transforms: &[("a", compose_a)],
};

static SIGNATURE_POLICY: Policy = Policy {
    allowed_tags: &[
        "a", "b", "strong", "i", "em", "u", "s", "del", "p", "br", "div", "span", "img", "ul", "ol", "li", "h1", "h2",
        "h3", "hr", "table", "thead", "tbody", "tr", "th", "td", "font", "center",
    ],
    allowed_attrs: &[
        ("*", &["style", "class"]),
        ("a", &["href", "target", "rel"]),
        ("img", &["src", "alt", "width", "height"]),
        ("font", &["color", "size", "face"]),
        ("td", &["colspan", "rowspan", "align", "valign", "width", "height", "bgcolor"]),
        ("th", &["colspan", "rowspan", "align", "valign"]),
        ("table", &["width", "cellpadding", "cellspacing", "border", "align", "bgcolor"]),
    ],
    allowed_schemes: &["https", "mailto"],
    schemes_by_tag: &[("img", &["https", "data"])],
    transforms: &[("a", compose_a)],
};

// ── Linear paired-tag scanning (port of scanPaired) ──────────────────────────────────

fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from.saturating_add(needle.len()) > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Find `<name\b` case-insensitively from `from`.
fn find_open(hay: &[u8], name: &[u8], from: usize) -> Option<usize> {
    let mut pos = from;
    loop {
        let i = find_ci(hay, name, pos)?;
        let after = i + name.len();
        if hay.get(after).is_none_or(|b| !is_word_byte(*b)) {
            return Some(i);
        }
        pos = i + 1;
    }
}

/// Replace every `<tag …>content</tag>` span in linear time.
pub fn scan_paired(
    s: &str,
    open_name: &str,
    close_literal: &str,
    mut transform: impl FnMut(&str, &str, &str) -> String,
) -> String {
    let b = s.as_bytes();
    let open = format!("<{open_name}");
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    let mut search = 0;
    while let Some(m) = find_open(b, open.as_bytes(), search) {
        let Some(gt) = s[m..].find('>').map(|i| m + i) else { break };
        let content_start = gt + 1;
        let Some(close_idx) = find_ci(b, close_literal.as_bytes(), content_start) else { break };
        let span_end = close_idx + close_literal.len();
        out.push_str(&s[pos..m]);
        out.push_str(&transform(&s[m..content_start], &s[content_start..close_idx], &s[close_idx..span_end]));
        pos = span_end;
        search = span_end;
    }
    out.push_str(&s[pos..]);
    out
}

/// Strip MSO-positive conditional comments, keeping `<!--[if !mso]>` blocks.
fn strip_mso_conditionals(hc: &str) -> String {
    const END: &str = "<![endif]-->";
    let b = hc.as_bytes();
    let mut out = String::with_capacity(hc.len());
    let mut pos = 0;
    let mut search = 0;
    while let Some(m) = find_ci(b, b"<!--[if", search) {
        let after = m + "<!--[if".len();
        let mut k = after;
        while k < b.len() && (b[k] as char).is_ascii_whitespace() {
            k += 1;
        }
        if b.get(k) == Some(&b'!') {
            search = after;
            continue;
        }
        let Some(rb) = hc[after..].find(']').map(|i| after + i) else { break };
        if b.get(rb + 1) != Some(&b'>') {
            search = after;
            continue;
        }
        let Some(end_idx) = find_ci(b, END.as_bytes(), rb + 2) else { break };
        out.push_str(&hc[pos..m]);
        pos = end_idx + END.len();
        search = pos;
    }
    out.push_str(&hc[pos..]);
    out
}

/// Strip `<head>`, keeping only its `<style>` blocks (MSO conditionals removed first).
pub fn strip_email_head(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    scan_paired(html, "head", "</head>", |_, head, _| {
        let no_mso = strip_mso_conditionals(head);
        let mut styles = String::new();
        scan_paired(&no_mso, "style", "</style>", |open, content, close| {
            styles.push_str(open);
            styles.push_str(content);
            styles.push_str(close);
            String::new()
        });
        styles
    })
}

static EXTERNAL_STYLE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)url\s*\(\s*(?:'https?://[^)]*'|"https?://[^)]*"|https?://[^)]*)\s*\)"#).expect("regex")
});
static DARK_MEDIA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)@media(?-u:\b)[^{]*prefers-color-scheme\s*:\s*dark[^{]*\{(?:[^{}]|\{[^{}]*\})*\}").expect("regex")
});
static DATA_OG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[[^\]]*data-og[^\]]*\][^{]*\{[^}]*\}").expect("regex"));
static COLOR_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?-u:\b)color-scheme\s*:[^;!}]+;?").expect("regex"));
static FILTER_INVERT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?-u:\b)filter\s*:\s*invert\([^)]*\)[^;]*;?").expect("regex"));

fn strip_dark_mode_css(css: &str) -> String {
    let out = DARK_MEDIA.replace_all(css, "");
    let out = DATA_OG.replace_all(&out, "");
    let out = COLOR_SCHEME.replace_all(&out, "");
    FILTER_INVERT.replace_all(&out, "").into_owned()
}

/// Sanitize an incoming email body (permissive but safe).
pub fn sanitize_email(html: &str) -> String {
    let sanitized = sanitize(&strip_email_head(html), &EMAIL_POLICY);
    let stripped = scan_paired(&sanitized, "style", "</style>", |o, c, cl| {
        format!("{o}{}{cl}", EXTERNAL_STYLE_URL.replace_all(c, "url()"))
    });
    let upgraded = scan_paired(&stripped, "style", "</style>", |o, c, cl| format!("{o}{}{cl}", upgrade_style_urls(c)));
    scan_paired(&upgraded, "style", "</style>", |o, c, cl| format!("{o}{}{cl}", strip_dark_mode_css(c)))
}

/// Sanitize a user-authored compose body.
pub fn sanitize_compose_body(html: &str) -> String {
    if html.is_empty() { String::new() } else { sanitize(html, &COMPOSE_POLICY) }
}

static TEXT_POLICY: Policy =
    Policy { allowed_tags: &[], allowed_attrs: &[], allowed_schemes: &[], schemes_by_tag: &[], transforms: &[] };

/// `sanitizeHtml(html, { allowedTags: [], allowedAttributes: {} })`: text only, still escaped.
pub fn strip_tags(html: &str) -> String {
    if html.is_empty() { String::new() } else { sanitize(html, &TEXT_POLICY) }
}

/// Sanitize a user-authored signature (stricter than email bodies).
pub fn sanitize_signature(html: &str) -> String {
    if html.is_empty() { String::new() } else { sanitize(html, &SIGNATURE_POLICY) }
}

static ANCHOR_HREF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(<a\b[^>]*?\s)href=(?:"([^"']*)"|'([^"']*)')"#).expect("regex"));

/// Rewrite anchor hrefs in already-cached HTML (applied at serve time).
pub fn rewrite_anchor_hrefs(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    ANCHOR_HREF
        .replace_all(html, |c: &Captures| {
            let pre = &c[1];
            let (q, raw) = match (c.get(2), c.get(3)) {
                (Some(v), _) => ('"', v.as_str()),
                (_, Some(v)) => ('\'', v.as_str()),
                _ => return c[0].to_owned(),
            };
            match normalize_href(raw) {
                None => pre.to_owned(),
                Some(n) if n == raw => c[0].to_owned(),
                Some(n) => format!("{pre}href={q}{n}{q}"),
            }
        })
        .into_owned()
}

static EBAY_SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(<img\b[^>]*?\s)src=(?:"(https://svcs\.ebay\.com/imageser/[^"']*)"|'(https://svcs\.ebay\.com/imageser/[^"']*)')"#)
        .expect("regex")
});

/// Rewrite eBay imageser src URLs remaining in cached HTML.
pub fn rewrite_ebay_imageser_urls(html: &str) -> String {
    if html.is_empty() || !html.contains("svcs.ebay.com/imageser") {
        return html.to_owned();
    }
    EBAY_SRC
        .replace_all(html, |c: &Captures| {
            let (q, url) = match (c.get(2), c.get(3)) {
                (Some(v), _) => ('"', v.as_str()),
                (_, Some(v)) => ('\'', v.as_str()),
                _ => return c[0].to_owned(),
            };
            match ebay_direct(&url.replace("&amp;", "&")) {
                Some(direct) => format!("{}src={q}{direct}{q}", &c[1]),
                None => c[0].to_owned(),
            }
        })
        .into_owned()
}

static REMOTE_PATTERNS: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        Regex::new(r#"(?i)<img\b[^>]*\ssrc=["']https?://"#).expect("regex"),
        Regex::new(r#"(?i)<img\b[^>]*\ssrcset=["'][^"']*https?://"#).expect("regex"),
        Regex::new(r#"(?i)\sbackground=["']https?://"#).expect("regex"),
        Regex::new(r#"(?i)url\(\s*['"]?https?://"#).expect("regex"),
        Regex::new(r#"(?i)@import\s+["']https?://"#).expect("regex"),
    ]
});

/// True if the HTML references remote images or stylesheets.
pub fn has_remote_images(html: &str) -> bool {
    !html.is_empty() && REMOTE_PATTERNS.iter().any(|r| r.is_match(html))
}

static BLOCK_IMG_SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(<img\b[^>]*?)\ssrc=(?:"(https?://[^\s"']*)"|'(https?://[^\s"']*)')"#).expect("regex")
});
static WIDTH_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?-u:\b)width=["']?(\d+)["']?"#).expect("regex"));
static HEIGHT_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?-u:\b)height=["']?(\d+)["']?"#).expect("regex"));
static BLOCK_SRCSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(<img\b[^>]*?)\ssrcset=(?:"([^"']*)"|'([^"']*)')"#).expect("regex"));
static HAS_REMOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)https?://").expect("regex"));
static BLOCK_BACKGROUND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\s)background=(?:"(https?://[^\s"']*)"|'(https?://[^\s"']*)')"#).expect("regex")
});
static STYLE_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\sstyle="([^"]*)""#).expect("regex"));
static REMOTE_CSS_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)url\(\s*(?:'https?://[^'")]+'|"https?://[^'")]+"|https?://[^'")]+)\s*\)"#).expect("regex")
});
static IMPORT_QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)@import\s+["']https?://[^"']*["']\s*;?"#).expect("regex"));
static IMPORT_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)@import\s+url\(\s*["']?https?://[^"')]*["']?\s*\)\s*;?"#).expect("regex"));

fn digits_prefix(s: &str) -> Option<u64> {
    let d: String = s.chars().take_while(char::is_ascii_digit).collect();
    d.parse().ok()
}

/// Rewrite remote image references so the browser makes no network requests (serve-time only).
pub fn block_remote_images(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    let out = BLOCK_IMG_SRC.replace_all(html, |c: &Captures| {
        let pre = &c[1];
        let remote = c.get(2).or_else(|| c.get(3)).map_or("", |m| m.as_str());
        let w = WIDTH_ATTR.captures(pre).and_then(|m| digits_prefix(&m[1])).unwrap_or(600);
        let h = HEIGHT_ATTR.captures(pre).and_then(|m| digits_prefix(&m[1])).unwrap_or(200);
        let svg = encode_uri_component(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\"><rect width=\"100%\" height=\"100%\" fill=\"#e8e8e8\"/></svg>"
        ));
        let encoded = remote.replace('&', "&amp;").replace('"', "&quot;");
        format!("{pre} data-mailflow-remote-src=\"{encoded}\" data-mailflow-remote-blocked=\"true\" src=\"data:image/svg+xml,{svg}\"")
    });
    let out = BLOCK_SRCSET.replace_all(&out, |c: &Captures| {
        let pre = &c[1];
        let (q, val) = match (c.get(2), c.get(3)) {
            (Some(v), _) => ('"', v.as_str()),
            (_, Some(v)) => ('\'', v.as_str()),
            _ => return c[0].to_owned(),
        };
        if HAS_REMOTE.is_match(val) { pre.to_owned() } else { format!("{pre} srcset={q}{val}{q}") }
    });
    let out = BLOCK_BACKGROUND.replace_all(&out, |c: &Captures| {
        let q = if c.get(2).is_some() { '"' } else { '\'' };
        format!("{}background={q}{q}", &c[1])
    });
    let out = STYLE_ATTR.replace_all(&out, |c: &Captures| {
        format!(" style=\"{}\"", REMOTE_CSS_URL.replace_all(&c[1], "url(\"data:,\")"))
    });
    scan_paired(&out, "style", "</style>", |open, content, close| {
        let blocked = IMPORT_QUOTED.replace_all(content, "");
        let blocked = IMPORT_URL.replace_all(&blocked, "");
        let blocked = REMOTE_CSS_URL.replace_all(&blocked, "url(\"data:,\")");
        format!("{open}{blocked}{close}")
    })
}

/// `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Remote-image preferences (`preferences.blockRemoteImages`, `preferences.imageWhitelist`).
pub fn should_block_remote_images(preferences: &serde_json::Value, from_email: Option<&str>) -> bool {
    use serde_json::Value;
    if preferences.get("blockRemoteImages") == Some(&Value::Bool(false)) {
        return false;
    }
    let sender = from_email.unwrap_or("").to_lowercase();
    let domain = if sender.contains('@') { sender.rsplit('@').next().unwrap_or("").to_owned() } else { String::new() };
    let list = |key: &str| -> Vec<String> {
        preferences
            .get("imageWhitelist")
            .and_then(|w| w.get(key))
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| crate::text::js_string(v).to_lowercase()).collect())
            .unwrap_or_default()
    };
    if !sender.is_empty() && list("addresses").contains(&sender) {
        return false;
    }
    if !domain.is_empty() && list("domains").iter().any(|v| domain == *v || domain.ends_with(&format!(".{v}"))) {
        return false;
    }
    true
}
