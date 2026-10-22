//! vCard behaviour must match the Node backend (tests/differential/vcard.expected.json).

use inboxora_pim::vcard;
use serde_json::Value;

#[test]
fn vcard_matches_node() {
    let raw =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/differential/vcard.expected.json"))
            .unwrap();
    let t: Value = serde_json::from_str(&raw).unwrap();
    let mut failures = Vec::new();
    for c in t["parse"].as_array().unwrap() {
        let input = c["input"].as_str().unwrap();
        let out = serde_json::to_value(vcard::parse(input)).unwrap();
        if out != c["out"] {
            failures.push(format!("parse {input:?}\n  node: {}\n  rust: {out}", c["out"]));
        }
    }
    for c in t["generate"].as_array().unwrap() {
        let out = vcard::generate(&c["input"]);
        if out != c["out"].as_str().unwrap() {
            failures.push(format!("generate {}\n  node: {:?}\n  rust: {out:?}", c["input"], c["out"]));
        }
    }
    for c in t["merge"].as_array().unwrap() {
        let out = vcard::merge(c["raw"].as_str().unwrap(), &c["contact"]);
        if out != c["out"].as_str().unwrap() {
            failures.push(format!("merge {:?} {}\n  node: {:?}\n  rust: {out:?}", c["raw"], c["contact"], c["out"]));
        }
    }
    for c in t["dates"].as_array().unwrap() {
        let input = c["input"].as_str().unwrap();
        let (a, b) = (vcard::normalize_vcard_date(input, true), vcard::normalize_vcard_date(input, false));
        if serde_json::json!(a) != c["out"] || serde_json::json!(b) != c["strict"] {
            failures.push(format!("date {input:?}: node {} {} rust {a:?} {b:?}", c["out"], c["strict"]));
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}
