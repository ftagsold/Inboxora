//! DAV response XML handling, compatible with the Node clients.
//!
//! The Node CardDAV/CalDAV clients read responses through fast-xml-parser 5 (`ignoreAttributes:
//! false, removeNSPrefix: true, trimValues: false`) and check them with its `XMLValidator`.
//! That parser is lenient (a closing tag pops whatever is open), types leaf values (`"7"` → 7,
//! `"true"` → true) and keeps whitespace text, and the clients' extraction logic depends on all
//! of it. This module ports the parser's scanning loop, its object compression, the validator
//! and the clients' extraction helpers so both backends discover and import the same
//! collections and resources. Differential tests: `tests/differential/dav.expected.json`.

use std::collections::HashMap;
use std::sync::LazyLock;

use inboxora_core::json::{js_float, js_number_string};
use regex::Regex;
use serde_json::{Map, Value};

// ── fast-xml-parser: scanning ─────────────────────────────────────────────────────────

enum Child {
    Text(Value),
    Element { name: String, node: usize, attrs: Option<Map<String, Value>> },
}

struct Node {
    tag: String,
    children: Vec<Child>,
}

fn type_error(prop: &str) -> String {
    format!("Cannot read properties of undefined (reading '{prop}')")
}

fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// strnum 2.4 `toNumber` (fast-xml-parser's `parseTagValue`), with booleans first.
fn parse_value(val: &str) -> Value {
    let t = js_trim(val);
    match t {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    to_number(val).unwrap_or_else(|| Value::String(val.to_owned()))
}

/// JS `Number(string)` for an already-trimmed string.
pub fn js_number(t: &str) -> f64 {
    static DEC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$").unwrap());
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |p: &str, r: u32| {
        t.strip_prefix(p)
            .or_else(|| t.strip_prefix(&p.to_uppercase()))
            .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_digit(r)))
            .map(|d| d.chars().fold(0f64, |acc, c| acc * f64::from(r) + f64::from(c.to_digit(r).unwrap_or(0))))
    };
    if let Some(v) = radix("0x", 16).or_else(|| radix("0b", 2)).or_else(|| radix("0o", 8)) {
        return v;
    }
    if DEC.is_match(t) { t.parse().unwrap_or(f64::NAN) } else { f64::NAN }
}

fn num(f: f64) -> Value {
    if f == 0.0 {
        return Value::from(0);
    }
    js_float(f)
}

fn trim_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_owned();
    }
    let s = s.trim_end_matches('0');
    if s == "." {
        "0".into()
    } else if let Some(rest) = s.strip_prefix('.') {
        format!("0.{rest}")
    } else {
        s.strip_suffix('.').unwrap_or(s).to_owned()
    }
}

fn to_number(s: &str) -> Option<Value> {
    static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-+]?0x[a-fA-F0-9]+$").unwrap());
    static NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([\-+])?(0*)([0-9]*(\.[0-9]*)?)$").unwrap());
    static ENOT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^([-+])?(0*)([0-9]*(\.[0-9]*)?[eE][-+]?[0-9]+)$").unwrap());
    if s.is_empty() {
        return None;
    }
    let t = js_trim(s);
    if t.is_empty() {
        return None;
    }
    if t == "0" {
        return Some(Value::from(0));
    }
    let chars: Vec<char> = s.chars().collect();
    if HEX.is_match(t) {
        // parseInt(t, 16)
        let (neg, digits) = match t.as_bytes()[0] {
            b'-' => (true, &t[3..]),
            b'+' => (false, &t[3..]),
            _ => (false, &t[2..]),
        };
        let v = digits.chars().fold(0f64, |acc, c| acc * 16.0 + f64::from(c.to_digit(16).unwrap_or(0)));
        return Some(num(if neg { -v } else { v }));
    }
    if !js_number(t).is_finite() {
        return None;
    }
    if t.contains('e') || t.contains('E') {
        let m = ENOT.captures(t)?;
        let sign = m.get(1).map_or("", |x| x.as_str());
        let lz = m.get(2).map_or("", |x| x.as_str());
        let rest = m.get(3).map_or("", |x| x.as_str());
        let e_char = if rest.contains('e') { 'e' } else { 'E' };
        let idx = if sign.is_empty() { lz.len() } else { lz.len() + 1 };
        let adjacent = chars.get(idx) == Some(&e_char);
        if lz.len() > 1 && adjacent {
            return None;
        } else if lz.len() == 1 && (rest.starts_with(&format!(".{e_char}")) || rest.starts_with(e_char)) {
            return Some(num(js_number(t)));
        } else if !lz.is_empty() {
            return if adjacent { None } else { Some(num(js_number(&format!("{sign}{rest}")))) };
        }
        return Some(num(js_number(t)));
    }
    let m = NUM.captures(t)?;
    let sign = m.get(1).map_or("", |x| x.as_str());
    let lz = m.get(2).map_or("", |x| x.as_str());
    let trimmed = trim_zeros(m.get(3).map_or("", |x| x.as_str()));
    let n = js_number(t);
    let parsed = js_number_string(n);
    if n == 0.0 {
        return Some(num(n));
    }
    if parsed.contains('e') || parsed.contains('E') {
        return Some(num(n));
    }
    if t.contains('.') {
        return (parsed == "0" || parsed == trimmed || parsed == format!("{sign}{trimmed}")).then(|| num(n));
    }
    let candidate = if lz.is_empty() { t.to_owned() } else { trimmed };
    let ok = if lz.is_empty() {
        candidate == parsed || candidate == format!("{sign}{parsed}")
    } else {
        candidate == parsed || format!("{sign}{candidate}") == parsed
    };
    ok.then(|| num(n))
}

/// JS `parseInt(s, radix)` for a numeric character reference body.
fn js_parse_int(s: &str, radix: u32) -> Option<i64> {
    let s = s.trim_start_matches(is_js_space);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let s = if radix == 16 { s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s) } else { s };
    let digits: String = s.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return None;
    }
    let v = i64::from_str_radix(&digits[..digits.len().min(15)], radix).ok()?;
    let v = if digits.len() > 15 { i64::MAX } else { v };
    Some(if neg { -v } else { v })
}

/// A numeric reference with `numericAllowed: false`: NUL, surrogates and (XML 1.0) C0
/// controls other than tab/LF/CR are removed; everything else is left as written.
fn ncr_removed(token: &str, xml11: bool) -> bool {
    let body = &token[1..];
    let cp = match body.as_bytes().first() {
        Some(b'x' | b'X') => js_parse_int(&body[1..], 16),
        _ => js_parse_int(body, 10),
    };
    match cp {
        Some(cp) if (0..=0x10FFFF).contains(&cp) => {
            cp == 0
                || (0xD800..=0xDFFF).contains(&cp)
                || (!xml11 && (0x01..=0x1F).contains(&cp) && ![0x09, 0x0A, 0x0D].contains(&cp))
        }
        _ => false,
    }
}

/// `@nodable/entities` with the XML set plus DOCTYPE-declared entities: a single pass over
/// `&name;` tokens (at most 32 characters); numeric references are left alone except the
/// prohibited ones, which are removed.
fn decode_entities_v(s: &str, extra: &HashMap<String, String>, xml11: bool) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j] != ';' && j - i <= 32 {
            j += 1;
        }
        if j >= chars.len() || chars[j] != ';' || j == i + 1 {
            out.push('&');
            i += 1;
            continue;
        }
        let token: String = chars[i + 1..j].iter().collect();
        if token.starts_with('#') {
            if ncr_removed(&token, xml11) {
                i = j + 1;
            } else {
                out.push('&');
                i += 1;
            }
            continue;
        }
        let rep = match token.as_str() {
            "lt" => Some("<"),
            "gt" => Some(">"),
            "amp" => Some("&"),
            "quot" => Some("\""),
            "apos" => Some("'"),
            t => extra.get(t).map(String::as_str),
        };
        match rep {
            Some(r) => {
                out.push_str(r);
                i = j + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

struct Parser<'a> {
    x: &'a [char],
    nodes: Vec<Node>,
    entities: HashMap<String, String>,
    xml11: bool,
}

impl Parser<'_> {
    fn find(&self, needle: &str, from: usize) -> Option<usize> {
        let n: Vec<char> = needle.chars().collect();
        (from..self.x.len().saturating_sub(n.len() - 1)).find(|&k| self.x[k..k + n.len()] == n[..])
    }

    fn closing_index(&self, needle: &str, from: usize, err: &str) -> Result<usize, String> {
        self.find(needle, from).map(|k| k + needle.chars().count() - 1).ok_or_else(|| err.to_owned())
    }

    fn substring(&self, a: usize, b: usize) -> String {
        let (a, b) = (a.min(self.x.len()), b.min(self.x.len()));
        if a >= b { String::new() } else { self.x[a..b].iter().collect() }
    }

    /// `tagExpWithClosingIndex`: the tag expression up to `close` outside quotes (tabs → spaces).
    fn tag_exp(&self, from: usize, close: &str) -> Option<(String, usize)> {
        let c: Vec<char> = close.chars().collect();
        let mut quote: Option<char> = None;
        let mut out = String::new();
        for k in from..self.x.len() {
            let ch = self.x[k];
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
                out.push(ch);
            } else if ch == '"' || ch == '\'' {
                quote = Some(ch);
                out.push(ch);
            } else if ch == c[0] && (c.len() == 1 || self.x.get(k + 1) == c.get(1)) {
                return Some((out, k));
            } else if ch == '\t' {
                out.push(' ');
            } else {
                out.push(ch);
            }
        }
        None
    }

    /// `readTagExp` → (tagName, tagExp, closeIndex, attrExpPresent).
    fn read_tag(&self, i: usize, remove_ns: bool, close: &str) -> Option<(String, String, usize, bool)> {
        let (data, close_index) = self.tag_exp(i + 1, close)?;
        let mut tag_name = data.clone();
        let mut tag_exp = data.clone();
        let mut attr_present = true;
        if let Some((pos, c)) = data.char_indices().find(|(_, c)| is_js_space(*c)) {
            tag_name = data[..pos].to_owned();
            tag_exp = data[pos + c.len_utf8()..].trim_start_matches(is_js_space).to_owned();
        }
        if remove_ns && let Some(colon) = tag_name.find(':') {
            tag_name = tag_name[colon + 1..].to_owned();
            attr_present = tag_name != data[colon + 1..];
        }
        Some((tag_name, tag_exp, close_index, attr_present))
    }

    fn attributes(&self, exp: &str) -> Option<Map<String, Value>> {
        static ATTR: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"([^\s=]+)\s*(=\s*(['"])([\s\S]*?)['"])?"#).unwrap());
        let mut attrs = Map::new();
        // The JS pattern back-references the opening quote; emulate it by scanning matches.
        let mut pos = 0;
        while pos < exp.len() {
            let Some(m) = ATTR.find_at(exp, pos) else { break };
            let caps = ATTR.captures_at(exp, m.start()).filter(|c| c.get(0).map(|g| g.start()) == Some(m.start()));
            let Some(caps) = caps else { break };
            let name = caps.get(1).map_or("", |x| x.as_str());
            let mut end = m.end();
            let mut value: Option<String> = None;
            if let (Some(eq), Some(q)) = (caps.get(2), caps.get(3)) {
                // Re-scan for the matching closing quote (`\3`).
                let quote = q.as_str();
                let body_start = q.end();
                match exp[body_start..].find(quote) {
                    Some(close) => {
                        value = Some(exp[body_start..body_start + close].to_owned());
                        end = body_start + close + 1;
                    }
                    None => end = eq.start(),
                }
            }
            let resolved = resolve_ns(name);
            if !resolved.is_empty()
                && let Some(v) = value
            {
                attrs.insert(format!("@_{resolved}"), Value::String(decode_entities_v(&v, &self.entities, self.xml11)));
            }
            pos = end.max(m.start() + 1);
            while pos < exp.len() && !exp.is_char_boundary(pos) {
                pos += 1;
            }
        }
        (!attrs.is_empty()).then_some(attrs)
    }

    /// `saveTextToParentTag` with `parseTextData`.
    fn save_text(&mut self, text: &mut String, parent: Option<usize>) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        let Some(p) = parent else { return Err(type_error("child")) };
        let decoded = decode_entities_v(text, &self.entities, self.xml11);
        let val = if js_trim(&decoded) == decoded { parse_value(&decoded) } else { Value::String(decoded) };
        if val != Value::String(String::new()) {
            self.nodes[p].children.push(Child::Text(val));
        }
        text.clear();
        Ok(())
    }

    fn add_child(
        &mut self,
        parent: Option<usize>,
        name: String,
        attrs: Option<Map<String, Value>>,
    ) -> Result<usize, String> {
        let Some(p) = parent else { return Err(type_error("addChild")) };
        let id = self.nodes.len();
        self.nodes.push(Node { tag: name.clone(), children: Vec::new() });
        self.nodes[p].children.push(Child::Element { name, node: id, attrs });
        Ok(id)
    }

    /// The internal subset of a DOCTYPE: collect simple `<!ENTITY name "value">` declarations.
    fn read_doctype(&mut self, i: usize) -> usize {
        static ENTITY: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"<!ENTITY\s+([^\s%"']+)\s+(?:"([^"]*)"|'([^']*)')\s*>"#).unwrap());
        let mut depth = 1;
        let mut k = i + 9;
        while k < self.x.len() {
            match self.x[k] {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        let body = self.substring(i, k + 1);
        for c in ENTITY.captures_iter(&body) {
            let value = c.get(2).or_else(|| c.get(3)).map_or("", |m| m.as_str());
            if !value.contains('<') && !value.contains('&') {
                self.entities.insert(c[1].to_owned(), value.to_owned());
            }
        }
        k
    }

    fn run(&mut self) -> Result<(), String> {
        let len = self.x.len();
        let mut current: Option<usize> = Some(0);
        let mut stack: Vec<Option<usize>> = Vec::new();
        let mut text = String::new();
        let mut i = 0;
        while i < len {
            let ch = self.x[i];
            if ch != '<' {
                text.push(ch);
                i += 1;
                continue;
            }
            let c1 = self.x.get(i + 1).copied();
            let at = |k: usize| self.x.get(k).copied();
            if c1 == Some('/') {
                let close = self.closing_index(">", i, "Closing Tag is not closed.")?;
                if current.is_some() {
                    self.save_text(&mut text, current)?;
                }
                current = stack.pop().flatten();
                text.clear();
                i = close + 1;
            } else if c1 == Some('?') {
                let Some((name, exp, close, present)) = self.read_tag(i, false, "?>") else {
                    return Err("Pi Tag is not closed.".into());
                };
                self.save_text(&mut text, current)?;
                let attrs = self.attributes(&exp);
                if let Some(v) = attrs.as_ref().and_then(|a| a.get("@_version")) {
                    self.xml11 = js_number(js_trim(&js_string(v))) == 1.1;
                }
                let attrs = if name != exp && present { attrs } else { None };
                let id = self.add_child(current, name, attrs)?;
                self.nodes[id].children.push(Child::Text(Value::String(String::new())));
                i = close + 2;
            } else if c1 == Some('!') && at(i + 2) == Some('-') && at(i + 3) == Some('-') {
                i = self.closing_index("-->", i + 4, "Comment is not closed.")? + 1;
            } else if c1 == Some('!') && at(i + 2) == Some('D') {
                i = self.read_doctype(i) + 1;
            } else if c1 == Some('!') && at(i + 2) == Some('[') {
                let close = self.closing_index("]]>", i, "CDATA is not closed.")? - 2;
                let body = self.substring(i + 9, close);
                self.save_text(&mut text, current)?;
                let Some(p) = current else { return Err(type_error("tagname")) };
                let val = if body.is_empty() {
                    Value::String(String::new())
                } else if js_trim(&body) == body {
                    parse_value(&body)
                } else {
                    Value::String(body)
                };
                self.nodes[p].children.push(Child::Text(val));
                i = close + 3;
            } else {
                let Some((mut name, mut exp, close, mut present)) = self.read_tag(i, true, ">") else {
                    let ctx = self.substring(i.saturating_sub(50), i + 50);
                    return Err(format!("readTagExp returned undefined at position {i}. Context: \"{ctx}\""));
                };
                if let Some(p) = current
                    && !text.is_empty()
                    && p != 0
                {
                    self.save_text(&mut text, current)?;
                }
                let mut self_closing = false;
                if !exp.is_empty() && exp.ends_with('/') {
                    self_closing = true;
                    if name.ends_with('/') {
                        name.pop();
                        exp = name.clone();
                    } else {
                        exp.pop();
                    }
                    present = name != exp;
                }
                let attrs = if name != exp && present { self.attributes(&exp) } else { None };
                if self_closing {
                    self.add_child(current, name, attrs)?;
                } else {
                    stack.push(current);
                    let id = self.add_child(current, name, attrs)?;
                    current = Some(id);
                }
                text.clear();
                i = close + 1;
            }
        }
        Ok(())
    }

    /// `node2json` compression of a node's children.
    fn compress(&self, node: usize) -> Map<String, Value> {
        let mut out = Map::new();
        let mut text: Option<Value> = None;
        for child in &self.nodes[node].children {
            match child {
                Child::Text(v) => {
                    text = Some(match text.take() {
                        None => v.clone(),
                        Some(prev) => Value::String(format!("{}{}", js_string(&prev), js_string(v))),
                    });
                }
                Child::Element { name, node, attrs } => {
                    let mut val = Value::Object(self.compress(*node));
                    if let Some(attrs) = attrs {
                        if let Value::Object(m) = &mut val {
                            m.extend(attrs.clone());
                        }
                    } else if let Value::Object(m) = &val {
                        if m.len() == 1 && m.contains_key("#text") {
                            val = m["#text"].clone();
                        } else if m.is_empty() {
                            val = Value::String(String::new());
                        }
                    }
                    let key = if name == "__proto__" { "#__proto__".to_owned() } else { name.clone() };
                    match out.get_mut(&key) {
                        Some(Value::Array(a)) => a.push(val),
                        Some(existing) => {
                            let prev = existing.take();
                            *existing = Value::Array(vec![prev, val]);
                        }
                        None => {
                            out.insert(key, val);
                        }
                    }
                }
            }
        }
        match text {
            Some(Value::String(s)) if s.is_empty() => {}
            Some(t) => {
                out.insert("#text".into(), t);
            }
            None => {}
        }
        out
    }
}

/// `removeNSPrefix` for attribute names (`xmlns` declarations are dropped).
fn resolve_ns(name: &str) -> String {
    let parts: Vec<&str> = name.split(':').collect();
    if parts[0] == "xmlns" {
        return String::new();
    }
    if parts.len() == 2 { parts[1].to_owned() } else { name.to_owned() }
}

/// JS `String(value)` for parser values.
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), js_number_string),
        Value::String(s) => s.clone(),
        Value::Array(a) => {
            a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(",")
        }
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `new XMLParser({ ignoreAttributes: false, removeNSPrefix: true, trimValues: false }).parse(raw)`.
pub fn parse(raw: &str) -> Result<Value, String> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let chars: Vec<char> = normalized.chars().collect();
    let mut p = Parser {
        x: &chars,
        nodes: vec![Node { tag: "!xml".into(), children: Vec::new() }],
        entities: HashMap::new(),
        xml11: false,
    };
    p.run()?;
    let _ = &p.nodes[0].tag;
    Ok(Value::Object(p.compress(0)))
}

// ── fast-xml-parser: XMLValidator.validate ────────────────────────────────────────────

fn is_name_start(c: char) -> bool {
    matches!(c, ':' | 'A'..='Z' | 'a'..='z' | '_' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}')
}

fn is_name(s: &str) -> bool {
    let mut it = s.chars();
    it.next().is_some_and(is_name_start)
        && it.all(|c| {
            is_name_start(c)
                || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
        })
}

fn ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn validate_attr_string(s: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(\s*)([^\s=]+)(\s*=)?(\s*(['"])([\s\S]*?)['"])?"#).unwrap());
    let mut names = std::collections::HashSet::new();
    let mut pos = 0;
    while pos <= s.len() {
        let Some(c) = RE.captures_at(s, pos) else { break };
        let m = c.get(0).unwrap();
        let mut end = m.end();
        let mut has_value = c.get(4).is_some();
        // Emulate the `\5` back-reference: the value ends at the same quote it opened with.
        if let Some(q) = c.get(5) {
            match s[q.end()..].find(q.as_str()) {
                Some(close) => end = q.end() + close + 1,
                None => {
                    has_value = false;
                    end = c.get(4).map_or(m.end(), |v| v.start());
                }
            }
        }
        if c.get(1).is_some_and(|w| w.as_str().is_empty()) {
            return false;
        }
        if c.get(3).is_some() && !has_value {
            return false;
        }
        if c.get(3).is_none() {
            return false;
        }
        let name = c.get(2).map_or("", |x| x.as_str());
        if !is_name(name) || !names.insert(name.to_owned()) {
            return false;
        }
        if end == m.start() {
            end += 1;
        }
        pos = end;
        while pos < s.len() && !s.is_char_boundary(pos) {
            pos += 1;
        }
    }
    true
}

/// `XMLValidator.validate(raw) === true`.
pub fn validate(raw: &str) -> bool {
    let mut x: Vec<char> = raw.chars().collect();
    if x.first() == Some(&'\u{feff}') {
        x.remove(0);
    }
    let len = x.len();
    let at = |k: usize| x.get(k).copied();
    let read_pi = |mut i: usize| -> Result<usize, ()> {
        let start = i;
        while i < len {
            if x[i] == '?' || x[i] == ' ' {
                let name: String = x[start..i].iter().collect();
                if i > 5 && name == "xml" {
                    return Err(());
                } else if x[i] == '?' && at(i + 1) == Some('>') {
                    i += 1;
                    break;
                }
            }
            i += 1;
        }
        Ok(i)
    };
    let read_comment_cdata = |mut i: usize| -> usize {
        let seq = |k: usize, s: &str| s.chars().enumerate().all(|(n, c)| at(k + n) == Some(c));
        if len > i + 5 && at(i + 1) == Some('-') && at(i + 2) == Some('-') {
            i += 3;
            while i < len {
                if seq(i, "-->") {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else if len > i + 8 && seq(i + 1, "DOCTYPE") {
            let mut depth = 1;
            i += 8;
            while i < len {
                if x[i] == '<' {
                    depth += 1;
                } else if x[i] == '>' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                i += 1;
            }
        } else if len > i + 9 && seq(i + 1, "[CDATA[") {
            i += 8;
            while i < len {
                if seq(i, "]]>") {
                    i += 2;
                    break;
                }
                i += 1;
            }
        }
        i
    };
    let validate_amp = |mut i: usize| -> Option<usize> {
        i += 1;
        if at(i) == Some(';') {
            return None;
        }
        if at(i) == Some('#') {
            i += 1;
            let hex = at(i) == Some('x');
            if hex {
                i += 1;
            }
            while i < len {
                if x[i] == ';' {
                    return Some(i);
                }
                if !(if hex { x[i].is_ascii_hexdigit() } else { x[i].is_ascii_digit() }) {
                    break;
                }
                i += 1;
            }
            return None;
        }
        let mut count = 0;
        while i < len {
            if (x[i].is_ascii_alphanumeric() || x[i] == '_') && count < 20 {
                i += 1;
                count += 1;
                continue;
            }
            if x[i] == ';' {
                break;
            }
            return None;
        }
        Some(i)
    };
    let mut tags: Vec<String> = Vec::new();
    let mut tag_found = false;
    let mut reached_root = false;
    let mut i = 0;
    while i < len {
        if x[i] == '<' && at(i + 1) == Some('?') {
            match read_pi(i + 2) {
                Ok(n) => i = n,
                Err(()) => return false,
            }
        } else if x[i] == '<' {
            i += 1;
            if at(i) == Some('!') {
                i = read_comment_cdata(i) + 1;
                continue;
            }
            let mut closing = false;
            if at(i) == Some('/') {
                closing = true;
                i += 1;
            }
            let mut name = String::new();
            while i < len && !matches!(x[i], '>' | ' ' | '\t' | '\n' | '\r') {
                name.push(x[i]);
                i += 1;
            }
            let mut name = js_trim(&name).to_owned();
            if name.ends_with('/') {
                name.pop();
                i -= 1;
            }
            if !is_name(&name) {
                return false;
            }
            // readAttributeStr
            let mut attr = String::new();
            let mut quote: Option<char> = None;
            let mut closed = false;
            while i < len {
                let c = x[i];
                if c == '"' || c == '\'' {
                    match quote {
                        None => quote = Some(c),
                        Some(q) if q == c => quote = None,
                        Some(_) => {}
                    }
                } else if c == '>' && quote.is_none() {
                    closed = true;
                    break;
                }
                attr.push(c);
                i += 1;
            }
            if quote.is_some() {
                return false;
            }
            if attr.ends_with('/') {
                attr.pop();
                if !validate_attr_string(&attr) {
                    return false;
                }
                tag_found = true;
            } else if closing {
                if !closed || !js_trim(&attr).is_empty() {
                    return false;
                }
                match tags.pop() {
                    Some(open) if open == name => {
                        if tags.is_empty() {
                            reached_root = true;
                        }
                    }
                    _ => return false,
                }
            } else {
                if !validate_attr_string(&attr) || reached_root {
                    return false;
                }
                tags.push(name);
                tag_found = true;
            }
            i += 1;
            while i < len {
                if x[i] == '<' {
                    if at(i + 1) == Some('!') {
                        i = read_comment_cdata(i + 1) + 1;
                        continue;
                    } else if at(i + 1) == Some('?') {
                        match read_pi(i + 1) {
                            Ok(n) => i = n,
                            Err(()) => return false,
                        }
                    } else {
                        break;
                    }
                } else if x[i] == '&' {
                    match validate_amp(i) {
                        Some(n) => i = n,
                        None => return false,
                    }
                } else if reached_root && !ws(x[i]) {
                    return false;
                }
                i += 1;
            }
            if at(i) == Some('<') {
                i -= 1;
            }
        } else if !ws(x[i]) {
            return false;
        }
        i += 1;
    }
    tag_found && tags.is_empty()
}

// ── client extraction helpers (carddavClient.ts / caldavClient.ts / davXml.ts) ─────────

/// `toArray(x)`.
pub fn to_array(v: Option<&Value>) -> Vec<&Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().collect(),
        Some(x) => vec![x],
    }
}

/// `textOf(node)`: a string, an element's `#text`, else ''.
pub fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(m)) => m.get("#text").map_or_else(String::new, js_string),
        _ => String::new(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

fn has_2xx(s: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)2[0-9][0-9](?-u:\b)").unwrap());
    RE.is_match(s)
}

/// `Object.assign(target, source)` for parsed values (strings spread their characters).
fn assign(target: &mut Map<String, Value>, source: &Value) {
    match source {
        Value::Object(m) => {
            for (k, v) in m {
                target.insert(k.clone(), v.clone());
            }
        }
        Value::String(s) => {
            for (n, c) in s.encode_utf16().enumerate() {
                target.insert(n.to_string(), Value::String(String::from_utf16_lossy(&[c])));
            }
        }
        _ => {}
    }
}

/// The clients' `propsOf`: merge `prop` of every propstat whose string status is 2xx (or absent).
pub fn props_of(response: &Value) -> Map<String, Value> {
    let mut merged = Map::new();
    for ps in to_array(response.get("propstat")) {
        let status = match ps.get("status") {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        if !status.is_empty() && !has_2xx(status) {
            continue;
        }
        if let Some(p) = ps.get("prop").filter(|p| truthy(Some(p))) {
            assign(&mut merged, p);
        }
    }
    merged
}

/// externalCalendarSync's `propsOf`: the status is read through `textOf`.
pub fn props_of_sync(response: &Value) -> Map<String, Value> {
    let mut merged = Map::new();
    for ps in to_array(response.get("propstat")) {
        let status = ps.get("status");
        if (!truthy(status) || has_2xx(&text_of(status)))
            && let Some(p) = ps.get("prop").filter(|p| truthy(Some(p)))
        {
            assign(&mut merged, p);
        }
    }
    merged
}

/// `new URL(href, base).href`, or the href itself when it does not resolve.
pub fn absolute(href: &str, base: &str) -> String {
    url::Url::parse(base).and_then(|b| b.join(href)).map_or_else(|_| href.to_owned(), |u| u.to_string())
}

fn responses(xml: &Value) -> Vec<&Value> {
    to_array(xml.get("multistatus").and_then(|m| m.get("response")))
}

/// `extractHref(xml, key, base)`.
pub fn extract_href(xml_text: &str, key: &str, base: &str) -> Result<Option<String>, String> {
    let xml = parse(xml_text)?;
    let Some(response) = responses(&xml).first().copied() else { return Ok(None) };
    let props = props_of(response);
    let val = props.get(key);
    let href = match val {
        Some(v @ Value::Object(m)) if m.contains_key("href") => match m.get("href") {
            Some(Value::Null) | None => v,
            Some(h) => h,
        },
        Some(v) => v,
        None => &Value::Null,
    };
    let mut text = text_of(Some(href));
    if text.is_empty()
        && let Value::String(s) = href
    {
        text = s.clone();
    }
    Ok((!text.is_empty()).then(|| absolute(&text, base)))
}

/// `'name' in rt` for `props.resourcetype || {}`; a non-object throws like JS.
fn resourcetype_has(props: &Map<String, Value>, names: &[&str]) -> Result<Vec<bool>, String> {
    match props.get("resourcetype") {
        Some(Value::Object(m)) => Ok(names.iter().map(|n| m.contains_key(*n)).collect()),
        rt if !truthy(rt) => Ok(vec![false; names.len()]),
        Some(other) => Err(format!("Cannot use 'in' operator to search for '{}' in {}", names[0], js_string(other))),
        None => Ok(vec![false; names.len()]),
    }
}

/// `textOf(response.href) || response.href` as the string `absolute()` receives.
fn response_href(response: &Value) -> Option<String> {
    let text = text_of(response.get("href"));
    if !text.is_empty() {
        return Some(text);
    }
    match response.get("href") {
        Some(v) if truthy(Some(v)) => Some(js_string(v)),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressBook {
    pub url: String,
    pub display_name: String,
}

/// `parseAddressBooks(xml, base)`.
pub fn parse_address_books(xml_text: &str, base: &str) -> Result<Vec<AddressBook>, String> {
    let xml = parse(xml_text)?;
    let mut books = Vec::new();
    for response in responses(&xml) {
        let props = props_of(response);
        if !resourcetype_has(&props, &["addressbook"])?[0] {
            continue;
        }
        let Some(href) = response_href(response) else { continue };
        let name = text_of(props.get("displayname"));
        books.push(AddressBook {
            url: absolute(&href, base),
            display_name: if name.is_empty() { "Contacts".into() } else { name },
        });
    }
    Ok(books)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredCalendar {
    pub href: String,
    pub display_name: String,
    pub color: Option<String>,
}

/// `parseCalendars(xml, base)`.
pub fn parse_calendars(xml_text: &str, base: &str) -> Result<Vec<DiscoveredCalendar>, String> {
    static COLOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#[0-9a-fA-F]{6}").unwrap());
    let xml = parse(xml_text)?;
    let mut out = Vec::new();
    for response in responses(&xml) {
        let props = props_of(response);
        let has = resourcetype_has(&props, &["calendar", "schedule-inbox", "schedule-outbox"])?;
        if !has[0] || has[1] || has[2] {
            continue;
        }
        let Some(href) = response_href(response) else { continue };
        let color = js_trim(&text_of(props.get("calendar-color"))).to_owned();
        let name = text_of(props.get("displayname"));
        out.push(DiscoveredCalendar {
            href: absolute(&href, base),
            display_name: if name.is_empty() { "Calendar".into() } else { name },
            color: COLOR.is_match(&color).then(|| color.chars().take(7).collect()),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Card {
    pub href: String,
    pub etag: String,
    pub vcard: String,
}

/// carddavClient's `decodeXmlCharRefs`.
pub fn decode_xml_char_refs(s: &str) -> String {
    decode_refs(s, |cp| cp <= 0x10FFFF && !(0xD800..=0xDFFF).contains(&cp))
}

/// davXml's `decodeDavCharRefs` (code point 0 stays encoded).
pub fn decode_dav_char_refs(s: &str) -> String {
    decode_refs(s, |cp| cp > 0 && cp <= 0x10FFFF && !(0xD800..=0xDFFF).contains(&cp))
}

fn decode_refs(s: &str, allowed: impl Fn(u64) -> bool) -> String {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#([xX][0-9a-fA-F]+|[0-9]+);").unwrap());
    RE.replace_all(s, |c: &regex::Captures<'_>| {
        let code = &c[1];
        let cp = if code.starts_with(['x', 'X']) {
            u64::from_str_radix(&code[1..], 16).ok()
        } else {
            code.parse::<u64>().ok()
        };
        match cp.filter(|cp| allowed(*cp)).and_then(|cp| char::from_u32(cp as u32)) {
            Some(ch) => ch.to_string(),
            None => c[0].to_owned(),
        }
    })
    .into_owned()
}

/// `requireCompleteMultistatus(raw, parsed)`.
pub fn require_complete_multistatus(raw: &str, parsed: &Value) -> Result<(), String> {
    static BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)(?:403|5[0-9][0-9])(?-u:\b)").unwrap());
    let ms = parsed.get("multistatus");
    let shaped = matches!(ms, Some(Value::Object(_))) || ms == Some(&Value::String(String::new()));
    if !validate(raw) || !shaped {
        return Err("DAV server returned an invalid multistatus response".into());
    }
    let responses = match ms {
        Some(Value::Object(m)) => to_array(m.get("response")),
        _ => Vec::new(),
    };
    for response in responses {
        let mut statuses = vec![response.get("status")];
        statuses.extend(to_array(response.get("propstat")).into_iter().map(|p| p.get("status")));
        let bad = statuses.into_iter().any(|s| {
            let text = match s {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Object(m)) => match m.get("#text") {
                    Some(Value::String(t)) => t.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            BAD.is_match(&text)
        });
        if bad {
            return Err("DAV server returned an incomplete resource response".into());
        }
    }
    Ok(())
}

/// `parseCards(xml, base)`.
pub fn parse_cards(xml_text: &str, base: &str) -> Result<Vec<Card>, String> {
    static TRUNCATED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)507(?-u:\b)").unwrap());
    let xml = parse(xml_text)?;
    let responses = responses(&xml);
    if responses.iter().any(|r| TRUNCATED.is_match(&text_of(r.get("status")))) {
        return Err("CardDAV server returned a truncated address book response".into());
    }
    require_complete_multistatus(xml_text, &xml)?;
    let mut cards = Vec::new();
    for response in responses {
        let props = props_of(response);
        let vcard = js_trim(&decode_xml_char_refs(&text_of(props.get("address-data")))).to_owned();
        if vcard.is_empty() {
            continue;
        }
        // `textOf(response.href) || response.href`, stringified by `new URL()`.
        let text = text_of(response.get("href"));
        let href =
            if text.is_empty() { response.get("href").map_or_else(|| "undefined".into(), js_string) } else { text };
        cards.push(Card { href: absolute(&href, base), etag: text_of(props.get("getetag")).replace('"', ""), vcard });
    }
    Ok(cards)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every corpus document must be handled exactly like the Node clients handled it.
    #[test]
    fn matches_node_on_the_dav_corpus() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/differential/dav.expected.json");
        let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let base = "https://dav.example.test/base/dir/";
        let wrap = |r: Result<Value, String>| match r {
            Ok(v) => serde_json::json!({ "ok": v }),
            Err(e) => serde_json::json!({ "error": e }),
        };
        fn ser<T: serde::Serialize>(v: T) -> Value {
            serde_json::to_value(v).unwrap()
        }
        let mut failures = Vec::new();
        for (n, case) in cases.iter().enumerate() {
            let doc = case["doc"].as_str().unwrap();
            let tree = parse(doc);
            let got = serde_json::json!({
                "valid": validate(doc),
                "client": wrap(tree.clone()),
                "complete": tree.as_ref().ok().map(|t| wrap(require_complete_multistatus(doc, t).map(|()| Value::Bool(true)))),
                "principal": wrap(extract_href(doc, "current-user-principal", base).map(ser)),
                "bookHome": wrap(extract_href(doc, "addressbook-home-set", base).map(ser)),
                "calHome": wrap(extract_href(doc, "calendar-home-set", base).map(ser)),
                "books": wrap(parse_address_books(doc, base).map(ser)),
                "calendars": wrap(parse_calendars(doc, base).map(ser)),
                "cards": wrap(parse_cards(doc, base).map(ser)),
                "charRefs": decode_dav_char_refs(doc),
            });
            for key in [
                "valid",
                "client",
                "complete",
                "principal",
                "bookHome",
                "calHome",
                "books",
                "calendars",
                "cards",
                "charRefs",
            ] {
                if got[key] != case[key] {
                    failures.push(format!("#{n} {key}: {doc:?}\n  node: {}\n  rust: {}", case[key], got[key]));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} mismatches:\n{}",
            failures.len(),
            failures.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn values_are_typed_like_fast_xml_parser() {
        for (s, v) in [
            ("-0", Value::from(0)),
            ("+1.50", serde_json::json!(1.5)),
            ("007", Value::from(7)),
            ("1e3", Value::from(1000)),
            (".5", serde_json::json!(0.5)),
            ("-0x10", Value::from(-16)),
            (" 1 ", Value::from(" 1 ")),
            ("Infinity", Value::from("Infinity")),
            ("12345678901234567890", Value::from("12345678901234567890")),
            ("false", Value::Bool(false)),
            ("abc", Value::from("abc")),
        ] {
            let parsed = if js_trim(s) == s { parse_value(s) } else { Value::String(s.into()) };
            assert_eq!(parsed, v, "{s}");
        }
    }
}
