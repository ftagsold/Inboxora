//! `srcset` parsing (WHATWG algorithm, as implemented by the `parse-srcset` package) and
//! sanitize-html's re-serialisation.

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub url: String,
    pub w: Option<i64>,
    pub h: Option<i64>,
    pub d: Option<f64>,
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')
}

/// Parse candidates; candidates with invalid descriptors are dropped.
pub fn parse(input: &str) -> Vec<Candidate> {
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        while pos < chars.len() && (is_space(chars[pos]) || chars[pos] == ',') {
            pos += 1;
        }
        if pos >= chars.len() {
            return out;
        }
        let start = pos;
        while pos < chars.len() && !is_space(chars[pos]) {
            pos += 1;
        }
        let mut url: String = chars[start..pos].iter().collect();
        let mut descriptors: Vec<String> = Vec::new();
        if url.ends_with(',') {
            url = url.trim_end_matches(',').to_owned();
        } else {
            // Tokenize descriptors.
            let mut current = String::new();
            let mut in_parens = false;
            loop {
                let c = chars.get(pos).copied();
                if in_parens {
                    match c {
                        Some(')') => {
                            current.push(')');
                            in_parens = false;
                        }
                        Some(ch) => current.push(ch),
                        None => {
                            descriptors.push(std::mem::take(&mut current));
                            break;
                        }
                    }
                    pos += 1;
                    continue;
                }
                match c {
                    Some(ch) if is_space(ch) => {
                        if !current.is_empty() {
                            descriptors.push(std::mem::take(&mut current));
                        }
                        pos += 1;
                    }
                    Some(',') => {
                        pos += 1;
                        if !current.is_empty() {
                            descriptors.push(std::mem::take(&mut current));
                        }
                        break;
                    }
                    Some('(') => {
                        current.push('(');
                        in_parens = true;
                        pos += 1;
                    }
                    Some(ch) => {
                        current.push(ch);
                        pos += 1;
                    }
                    None => {
                        if !current.is_empty() {
                            descriptors.push(std::mem::take(&mut current));
                        }
                        break;
                    }
                }
            }
        }
        if let Some(c) = parse_descriptors(url, &descriptors) {
            out.push(c);
        }
    }
}

fn is_non_negative_integer(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_float(s: &str) -> bool {
    // ^-?(?:[0-9]+|[0-9]*\.[0-9]+)(?:[eE][+-]?[0-9]+)?$
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mant, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mant_ok = match mant.split_once('.') {
        Some((a, b)) => a.bytes().all(|x| x.is_ascii_digit()) && !b.is_empty() && b.bytes().all(|x| x.is_ascii_digit()),
        None => is_non_negative_integer(mant),
    };
    let exp_ok = exp.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        is_non_negative_integer(e)
    });
    mant_ok && exp_ok
}

fn parse_descriptors(url: String, descriptors: &[String]) -> Option<Candidate> {
    let (mut w, mut h, mut d) = (None, None, None);
    for desc in descriptors {
        let last = desc.chars().last()?;
        let value = &desc[..desc.len() - last.len_utf8()];
        match last {
            'w' if is_non_negative_integer(value) && w.is_none() && d.is_none() => {
                let v: i64 = value.parse().ok()?;
                if v == 0 {
                    return None;
                }
                w = Some(v);
            }
            'x' if is_float(value) && w.is_none() && d.is_none() && h.is_none() => {
                let v: f64 = value.parse().ok()?;
                if v < 0.0 {
                    return None;
                }
                d = Some(v);
            }
            'h' if is_non_negative_integer(value) && h.is_none() && d.is_none() => {
                let v: i64 = value.parse().ok()?;
                if v == 0 {
                    return None;
                }
                h = Some(v);
            }
            _ => return None,
        }
    }
    if h.is_some() && w.is_none() {
        return None;
    }
    Some(Candidate { url, w, h, d })
}

fn js_num(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e21 { format!("{}", f as i64) } else { format!("{f}") }
}

/// sanitize-html's `stringifySrcset` (zero descriptors are falsy and omitted).
pub fn stringify(candidates: &[Candidate]) -> String {
    candidates
        .iter()
        .map(|c| {
            let mut s = c.url.clone();
            if let Some(w) = c.w.filter(|v| *v != 0) {
                s.push_str(&format!(" {w}w"));
            }
            if let Some(h) = c.h.filter(|v| *v != 0) {
                s.push_str(&format!(" {h}h"));
            }
            if let Some(d) = c.d.filter(|v| *v != 0.0) {
                s.push_str(&format!(" {}x", js_num(d)));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_stringifies() {
        let c = parse("a.png 1x, b.png 2x,c.png 100w, data:image/png;base64,AA= 1.5x");
        assert_eq!(stringify(&c), "a.png 1x, b.png 2x, c.png 100w, data:image/png;base64,AA= 1.5x");
        assert_eq!(stringify(&parse("x.png 2.0x")), "x.png 2x");
        assert!(parse("x.png bogus").is_empty());
    }
}
