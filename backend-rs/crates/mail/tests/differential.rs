//! Differential tests: every case in tests/differential/*.expected.json was produced by the
//! Node backend (gen.mjs); the Rust port must reproduce it exactly unless the case is listed in
//! `DEVIATIONS` with a reason.

use inboxora_mail::{sanitize, snippet};
use serde_json::Value;

/// (input, function, reason) for intentional differences from the Node output.
const DEVIATIONS: &[(&str, &str, &str)] = &[
    (
        "<span style=\"color: red; /* comment */ padding: 1px\">comment</span>",
        "*",
        "postcss turns a CSS comment into a declaration printed as `undefined:undefined`; Rust drops the comment",
    ),
    (
        "<iframe src=\"x\"><img src=x onerror=alert(1)></iframe>",
        "buildSnippetFromHtml",
        "Node's snippet parser (htmlparser2 10) parses iframe content as markup; Rust treats it as raw text like browsers and htmlparser2 12",
    ),
    (
        "<textarea></textarea/><img src=x onerror=alert(1)>",
        "buildSnippetFromHtml",
        "htmlparser2 10 does not end <textarea> at `</textarea/>`; browsers (and Rust) do",
    ),
];

/// The Node snippet builder uses htmlparser2 10, which tokenizes raw-text, RCDATA and foreign
/// elements differently from the HTML spec (and from htmlparser2 12 used by the sanitizer). Rust
/// follows the spec for snippets, so snippet differences are expected only for such inputs.
fn snippet_version_difference(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    [
        "<iframe",
        "<textarea",
        "<xmp",
        "<title",
        "<noscript",
        "<math",
        "<svg",
        "<plaintext",
        "<noembed",
        "<noframes",
        "<script",
        "<style",
    ]
    .iter()
    .any(|t| lower.contains(t))
}

fn deviation(input: &str, func: &str) -> bool {
    (func == "buildSnippetFromHtml" && snippet_version_difference(input))
        || DEVIATIONS.iter().any(|(i, f, _)| *i == input && (*f == func || *f == "*"))
}

#[test]
fn sanitizer_matches_node() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/differential/sanitizer.expected.json"
    ))
    .unwrap();
    let cases: Vec<Value> = serde_json::from_str(&raw).unwrap();
    let mut failures = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let input = c["input"].as_str().unwrap();
        let s = |v: &Value| v.as_str().map(str::to_owned);
        let checks: Vec<(&str, Option<String>, Option<String>)> = vec![
            ("sanitizeEmail", s(&c["sanitizeEmail"]), Some(sanitize::sanitize_email(input))),
            ("sanitizeComposeBody", s(&c["sanitizeComposeBody"]), Some(sanitize::sanitize_compose_body(input))),
            ("sanitizeSignature", s(&c["sanitizeSignature"]), Some(sanitize::sanitize_signature(input))),
            ("stripEmailHead", s(&c["stripEmailHead"]), Some(sanitize::strip_email_head(input))),
            ("rewriteAnchorHrefs", s(&c["rewriteAnchorHrefs"]), Some(sanitize::rewrite_anchor_hrefs(input))),
            (
                "rewriteEbayImageserUrls",
                s(&c["rewriteEbayImageserUrls"]),
                Some(sanitize::rewrite_ebay_imageser_urls(input)),
            ),
            (
                "hasRemoteImages",
                Some(c["hasRemoteImages"].to_string()),
                Some(sanitize::has_remote_images(input).to_string()),
            ),
            ("blockRemoteImages", s(&c["blockRemoteImages"]), Some(sanitize::block_remote_images(input))),
            (
                "blockRemoteImagesSanitized",
                s(&c["blockRemoteImagesSanitized"]),
                Some(sanitize::block_remote_images(&sanitize::sanitize_email(input))),
            ),
            ("buildSnippetFromHtml", s(&c["buildSnippetFromHtml"]), Some(snippet::build_snippet_from_html(input))),
        ];
        for (func, expected, actual) in checks {
            let expected = expected.unwrap_or_default();
            let actual = actual.unwrap_or_default();
            if expected != actual && !deviation(input, func) {
                failures.push(format!(
                    "#{i} {func}\n  input:    {input:?}\n  node:     {expected:?}\n  rust:     {actual:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn text_helpers_match_node() {
    use inboxora_mail::headers::{self, Headers};
    let raw =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/differential/text.expected.json"))
            .unwrap();
    let t: Value = serde_json::from_str(&raw).unwrap();
    let mut failures = Vec::new();
    for c in t["snippetFromBody"].as_array().unwrap() {
        let out = snippet::snippet_from_body(c["text"].as_str().unwrap(), c["html"].as_str());
        if out != c["out"].as_str().unwrap() {
            failures.push(format!(
                "snippetFromBody {:?} / {:?}\n  node: {:?}\n  rust: {out:?}",
                c["text"], c["html"], c["out"]
            ));
        }
    }
    for c in t["decodeMimeWords"].as_array().unwrap() {
        let out = headers::decode_mime_words(c["input"].as_str().unwrap());
        if out != c["out"].as_str().unwrap() {
            failures.push(format!("decodeMimeWords {:?}\n  node: {:?}\n  rust: {out:?}", c["input"], c["out"]));
        }
    }
    for c in t["headers"].as_array().unwrap() {
        let input = c["input"].as_str().unwrap();
        let h = Headers::parse(input);
        let to = h.get("to").map(headers::parse_mailbox_list).unwrap_or_default();
        let got = serde_json::json!({
            "parsed": h.to_json(),
            "to": to,
            "category": headers::detect_category(&h),
            "bulk": headers::detect_bulk(&h),
            "delivery": headers::parse_delivery_addresses(&h),
            "raw": h.to_raw_string(),
        });
        for key in ["parsed", "to", "category", "bulk", "delivery", "raw"] {
            if got[key] != c[key] {
                failures.push(format!("headers.{key} {input:?}\n  node: {}\n  rust: {}", c[key], got[key]));
            }
        }
    }
    for c in t["buildHeaders"].as_array().unwrap() {
        let out = headers::build_headers_from_message(&c["input"]);
        if out != c["out"].as_str().unwrap() {
            failures.push(format!("buildHeadersFromMessage {}\n  node: {:?}\n  rust: {out:?}", c["input"], c["out"]));
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}
