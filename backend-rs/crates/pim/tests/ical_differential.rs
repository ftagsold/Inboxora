//! iCalendar behaviour must match ical.js / the Node backend (tests/differential/ical.expected.json).

use inboxora_pim::ical::{
    self,
    component::parse_recur,
    recur::{Recur, RecurIterator, time_from_string},
};
use serde_json::{Map, Value, json};

fn load() -> Value {
    let raw =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/differential/ical.expected.json"))
            .unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn corpus_docs() -> Vec<String> {
    // The documents are embedded in the expected file through `parsed`/`projections`; the raw
    // texts live in the corpus module, re-exported by gen.mjs as `docs` in projections' rows.
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/differential/ical.docs.json"))
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn recur_iterator_matches_ical_js() {
    let t = load();
    let mut failures = Vec::new();
    for c in t["iterator"].as_array().unwrap() {
        let (rule, start) = (c["rule"].as_str().unwrap(), c["start"].as_str().unwrap());
        let started = std::time::Instant::now();
        let got: Result<Vec<String>, String> = (|| {
            let data = parse_recur(rule)?;
            let r = Recur::from_data(&data);
            let s = time_from_string(start)?;
            let mut it = RecurIterator::new(r, &s).map_err(|e| e.0)?;
            let mut out = Vec::new();
            for _ in 0..30 {
                match it.next().map_err(|e| e.0)? {
                    Some(o) => out.push(o.to_js_string()),
                    None => break,
                }
            }
            Ok(out)
        })();
        if c.get("timeout").is_some() {
            // ical.js never returns for this rule; the port must terminate promptly instead.
            if started.elapsed() > std::time::Duration::from_secs(5) {
                failures.push(format!("{rule} @ {start}: Node hangs, Rust took {:?}", started.elapsed()));
            }
            continue;
        }
        let expected: Result<Vec<String>, String> = match c.get("error") {
            Some(e) => Err(e.as_str().unwrap().to_owned()),
            None => Ok(c["out"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect()),
        };
        let same = match (&got, &expected) {
            (Ok(a), Ok(b)) => a == b,
            (Err(_), Err(_)) => true,
            _ => false,
        };
        if !same {
            failures.push(format!("{rule} @ {start}\n  node: {expected:?}\n  rust: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn projections_match_node() {
    let t = load();
    let docs = corpus_docs();
    let mut failures = Vec::new();
    for c in t["projections"].as_array().unwrap() {
        let d = c["doc"].as_u64().unwrap() as usize;
        let from =
            chrono::DateTime::parse_from_rfc3339(c["from"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
        let to = chrono::DateTime::parse_from_rfc3339(c["to"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
        let mut row = Map::new();
        for (k, v) in [
            ("id", json!(format!("row-{d}"))),
            ("summary", json!("meta summary")),
            ("description", Value::Null),
            ("location", Value::Null),
            ("organizer", Value::Null),
            ("attendees", Value::Null),
            ("starts_at", json!("2026-01-01T00:00:00.000Z")),
            ("ends_at", json!("2026-01-01T01:00:00.000Z")),
            ("all_day", json!(false)),
            ("raw_ical", json!(docs[d])),
        ] {
            row.insert(k.into(), v);
        }
        let opts = ical::ProjectOptions { max_iterations: Some(5000), ..Default::default() };
        let got = ical::project_calendar_resource_with_status(&row, from, to, &opts);
        let got_json = match &got {
            Ok(s) => {
                let mut m = Map::new();
                m.insert(
                    "events".into(),
                    Value::Array(s.events.iter().map(|e| Value::Object(e.fields.clone())).collect()),
                );
                m.insert("truncated".into(), json!(s.truncated));
                m.insert("reason".into(), json!(s.reason));
                if let Some(e) = &s.error {
                    m.insert("error".into(), json!(e));
                }
                Value::Object(m)
            }
            Err(e) => json!({ "error": e }),
        };
        let mut expected = c.get("out").cloned().unwrap_or_else(|| json!({ "error": c["error"] }));
        // Error messages are implementation-specific; compare presence only.
        let strip = |v: &mut Value| {
            if let Some(o) = v.as_object_mut()
                && o.contains_key("error")
            {
                o.insert("error".into(), json!("<error>"));
            }
        };
        let mut got_cmp = got_json.clone();
        strip(&mut expected);
        strip(&mut got_cmp);
        if got_cmp != expected {
            failures.push(format!("doc {d} {from}..{to}\n  node: {expected}\n  rust: {got_cmp}"));
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn parse_truncate_merge_resources_match_node() {
    let t = load();
    let docs = corpus_docs();
    let mut failures = Vec::new();
    for (d, expected) in t["parsed"].as_array().unwrap().iter().enumerate() {
        let got = ical::parse_calendar_event(&docs[d]).map(|e| {
            json!({
                "uid": e.uid, "startsAt": e.starts_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "endsAt": e.ends_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "allDay": e.all_day,
                "timeZone": e.time_zone, "summary": e.summary, "description": e.description, "location": e.location,
                "url": e.url, "organizer": e.organizer, "attendees": e.attendees, "raw": e.raw,
            })
        });
        let got = got.unwrap_or(Value::Null);
        if &got != expected {
            failures.push(format!("parseCalendarEvent doc {d}\n  node: {expected}\n  rust: {got}"));
        }
    }
    for c in t["truncated"].as_array().unwrap() {
        let d = c["doc"].as_u64().unwrap() as usize;
        let got = ical::truncate_series_before(&docs[d], c["id"].as_str().unwrap())
            .map(|(raw, empty)| json!({ "raw": raw, "empty": empty }));
        let got = got.unwrap_or(Value::Null);
        if got != c["out"] {
            failures.push(format!("truncate doc {d} {}\n  node: {}\n  rust: {got}", c["id"], c["out"]));
        }
    }
    for c in t["merged"].as_array().unwrap() {
        let d = c["doc"].as_u64().unwrap() as usize;
        let got = ical::merge_calendar_resource(
            Some(&docs[d]),
            c["repl"].as_str().unwrap(),
            c["id"].as_str(),
            c["cancel"].as_bool().unwrap(),
        );
        match (&got, c.get("out")) {
            (Ok(g), Some(e)) if e.as_str() == Some(g.as_str()) => {}
            (Err(_), None) => {}
            _ => failures.push(format!(
                "merge doc {d} {}\n  node: {}\n  rust: {got:?}",
                c["id"],
                c.get("out").unwrap_or(&c["error"])
            )),
        }
    }
    for (d, c) in t["resources"].as_array().unwrap().iter().enumerate() {
        let got = ical::calendar_resources(&docs[d]);
        match (&got, c.get("out")) {
            (Ok(g), Some(e)) if json!(g) == *e => {}
            (Err(_), None) => {}
            _ => failures
                .push(format!("resources doc {d}\n  node: {}\n  rust: {got:?}", c.get("out").unwrap_or(&c["error"]))),
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
}
