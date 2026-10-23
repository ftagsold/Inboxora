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

#[test]
fn contact_helpers_match_node() {
    use inboxora_pim::contacts;
    let raw =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/differential/vcard.expected.json"))
            .unwrap();
    let t: Value = serde_json::from_str(&raw).unwrap();
    let mut failures = Vec::new();
    for c in t["rich"].as_array().unwrap() {
        let got = contacts::normalize_rich_fields(&c["input"]).map(Value::Object).unwrap_or(Value::Null);
        if got != c["out"] {
            failures.push(format!("rich {}\n  node: {}\n  rust: {got}", c["input"], c["out"]));
        }
    }
    let rows: Vec<Value> = serde_json::from_str(include_str!("../../../tests/differential/export-rows.json")).unwrap();
    for (k, got) in [
        ("google", contacts::contacts_to_google_csv(&rows)),
        ("outlook", contacts::contacts_to_outlook_csv(&rows)),
        ("vcard", contacts::contacts_to_vcard(&rows)),
    ] {
        if got != t["exports"][k].as_str().unwrap() {
            failures.push(format!("export {k}\n  node: {:?}\n  rust: {got:?}", t["exports"][k]));
        }
    }
    for c in t["googleCsv"].as_array().unwrap() {
        let got = Value::Array(contacts::parse_google_csv(c["input"].as_str().unwrap()));
        if got != c["out"] {
            failures.push(format!("googleCsv {:?}\n  node: {}\n  rust: {got}", c["input"], c["out"]));
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}
