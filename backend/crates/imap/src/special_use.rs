//! imapflow's special-use detection: RFC 6154 flags first, then its localized name tables
//! (`special_use.json`, extracted from imapflow's `lib/special-use.js`).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use unicode_normalization::UnicodeNormalization;

struct Tables {
    flags: Vec<&'static str>,
    names: HashMap<String, &'static str>,
    generic: HashSet<String>,
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    let v: serde_json::Value = serde_json::from_str(include_str!("special_use.json")).expect("special_use.json");
    let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
    let flags = v["flags"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).map(leak).collect();
    let mut names = HashMap::new();
    for (flag, list) in v["names"].as_object().into_iter().flatten() {
        let flag = leak(flag);
        for n in list.as_array().into_iter().flatten().filter_map(|n| n.as_str()) {
            names.insert(n.to_owned(), flag);
        }
    }
    let generic = v["generic"].as_array().into_iter().flatten().filter_map(|g| g.as_str()).map(str::to_owned).collect();
    Tables { flags, names, generic }
});

fn normalize(name: &str) -> String {
    name.to_lowercase().replace('\u{200e}', "").trim().nfkc().collect()
}

fn is_split(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '_' | '/' | '.' | ',' | '(' | ')' | '[' | ']')
}

/// Returns (flag, source) where source is `extension`, `name` or `name-guess`.
pub fn detect(has_extension: bool, flags: &HashSet<String>, name: &str) -> Option<(&'static str, &'static str)> {
    let t = &*TABLES;
    if has_extension && let Some(f) = t.flags.iter().find(|f| flags.contains(**f)) {
        return Some((f, "extension"));
    }
    let n = normalize(name);
    if let Some(f) = t.names.get(&n) {
        return Some((f, "name"));
    }
    let core: Vec<&str> = n.split(is_split).filter(|tok| !tok.is_empty() && !t.generic.contains(*tok)).collect();
    if core.len() == 1
        && core[0] != n
        && let Some(f) = t.names.get(core[0])
    {
        return Some((f, "name-guess"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_flags() {
        let none = HashSet::new();
        assert_eq!(detect(false, &none, "Sent Items"), Some(("\\Sent", "name")));
        assert_eq!(detect(false, &none, "Gesendete Objekte"), Some(("\\Sent", "name-guess")));
        assert_eq!(detect(false, &none, "Trash"), Some(("\\Trash", "name")));
        assert_eq!(detect(false, &none, "Projects"), None);
        let flags: HashSet<String> = ["\\Junk".to_owned()].into();
        assert_eq!(detect(true, &flags, "Whatever"), Some(("\\Junk", "extension")));
    }
}
