//! Security property: whatever the input, sanitizer output parsed the way a browser parses it
//! (full HTML5 tree construction) contains only allowlisted elements, no event-handler
//! attributes and no script-capable URLs.

use html5ever_dom::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use serde_json::Value;

const EMAIL_TAGS: &[&str] = &[
    "html",
    "head",
    "body",
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
];

fn check(node: &Handle, violations: &mut Vec<String>) {
    if let NodeData::Element { name, attrs, .. } = &node.data {
        let tag = name.local.to_string();
        if !EMAIL_TAGS.contains(&tag.as_str()) {
            violations.push(format!("element <{tag}>"));
        }
        for a in attrs.borrow().iter() {
            let key = a.name.local.to_string().to_ascii_lowercase();
            let value: String = a
                .value
                .chars()
                .filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control())
                .collect::<String>()
                .to_ascii_lowercase();
            if key.starts_with("on") {
                violations.push(format!("<{tag} {key}>"));
            }
            let url_attr = matches!(
                key.as_str(),
                "href" | "src" | "background" | "srcset" | "action" | "formaction" | "xlink:href"
            );
            if url_attr && (value.contains("javascript:") || value.contains("vbscript:")) {
                violations.push(format!("<{tag} {key}={value}>"));
            }
            if url_attr && tag != "img" && value.starts_with("data:") {
                violations.push(format!("<{tag} {key}=data:…>"));
            }
        }
    }
    for child in node.children.borrow().iter() {
        check(child, violations);
    }
}

#[test]
fn sanitized_output_is_safe_when_parsed_by_a_browser() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/differential/sanitizer.expected.json"
    ))
    .unwrap();
    let cases: Vec<Value> = serde_json::from_str(&raw).unwrap();
    let mut failures = Vec::new();
    for c in &cases {
        let input = c["input"].as_str().unwrap();
        for (label, out) in [
            ("email", inboxora_mail::sanitize::sanitize_email(input)),
            ("compose", inboxora_mail::sanitize::sanitize_compose_body(input)),
            ("signature", inboxora_mail::sanitize::sanitize_signature(input)),
            ("blocked", inboxora_mail::sanitize::block_remote_images(&inboxora_mail::sanitize::sanitize_email(input))),
        ] {
            let dom = html5ever_dom::parse_document(RcDom::default(), Default::default()).one(out.as_str());
            let mut v = Vec::new();
            check(&dom.document, &mut v);
            if !v.is_empty() {
                failures.push(format!("{label}: {input:?} → {out:?}: {v:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn checker_flags_unsafe_markup() {
    let dom = html5ever_dom::parse_document(RcDom::default(), Default::default())
        .one("<img src=x onerror=alert(1)><a href=' java\tscript:x'>a</a><script>x</script>");
    let mut v = Vec::new();
    check(&dom.document, &mut v);
    assert_eq!(v.len(), 3, "{v:?}");
}
