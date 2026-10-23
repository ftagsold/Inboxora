//! Streaming HTML events with `htmlparser2` 12 semantics, on top of the spec-compliant
//! html5ever tokenizer.
//!
//! The Node sanitizer and snippet builder consume htmlparser2 callbacks (`onopentag`,
//! `ontext`, `onclosetag`), whose tree rules differ from both a bare tokenizer and the full
//! HTML5 tree builder: opening some tags implicitly closes others (`<p>` closes an open `<p>`,
//! `<td>` closes `<td>`/`<th>`), an end tag closes every element above its match, a stray
//! `</p>`/`</br>` produces an element, and everything still open is closed at EOF. This module
//! reproduces those rules so the Rust sanitizer produces the same output.
//!
//! Tokenization itself follows the HTML spec (html5ever): raw-text elements (`script`,
//! `style`, `xmp`, `iframe`, `noembed`, `noframes`) and RCDATA elements (`title`, `textarea`)
//! switch tokenizer states outside SVG/MathML, exactly where htmlparser2 does.

use std::cell::RefCell;

use html5ever::TokenizerResult;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};

/// One parser event.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// An opening tag with attributes in source order (duplicates removed, first wins).
    Open { name: String, attrs: Vec<(String, String)> },
    /// A closing tag; `implied` when no explicit end tag closed it.
    Close { name: String, implied: bool },
    /// Text with entities decoded.
    Text(String),
}

fn implies_close(name: &str) -> &'static [&'static str] {
    const P: &[&str] = &["p"];
    const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6", "p"];
    const FORM: &[&str] = &["input", "option", "optgroup", "select", "button", "datalist", "textarea"];
    match name {
        "tr" => &["tr", "th", "td"],
        "th" => &["th"],
        "td" => &["thead", "th", "td"],
        "body" => &["head", "link", "script"],
        "a" => &["a"],
        "li" => &["li"],
        "p" => P,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => HEADINGS,
        "select" | "input" | "output" | "button" | "datalist" | "textarea" => FORM,
        "option" => &["option"],
        "optgroup" => &["optgroup", "option"],
        "dd" | "dt" => &["dd", "dt"],
        "address" | "article" | "aside" | "blockquote" | "details" | "div" | "dl" | "fieldset" | "figcaption"
        | "figure" | "footer" | "form" | "header" | "hr" | "main" | "nav" | "ol" | "pre" | "section" | "table"
        | "ul" => P,
        "rt" | "rp" => &["rt", "rp"],
        "tbody" | "tfoot" => &["thead", "tbody"],
        _ => &[],
    }
}

/// htmlparser2's void elements.
pub fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "br"
            | "col"
            | "command"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "isindex"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_integration_point(name: &str) -> bool {
    // Compared case-insensitively: html5ever lowercases tag names.
    matches!(name, "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml" | "foreignobject" | "desc" | "title")
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Html,
    Foreign,
}

/// Tree state (htmlparser2 `Parser`): open-element stack (top = last) and foreign contexts.
struct Tree<F: FnMut(Event)> {
    stack: Vec<String>,
    contexts: Vec<Ctx>,
    emit: F,
}

impl<F: FnMut(Event)> Tree<F> {
    fn in_foreign(&self) -> bool {
        self.contexts.last() == Some(&Ctx::Foreign)
    }

    fn pop(&mut self, implied: bool) {
        if let Some(el) = self.stack.pop() {
            if el == "svg" || el == "math" || is_integration_point(&el) {
                self.contexts.pop();
            }
            (self.emit)(Event::Close { name: el, implied });
        }
    }

    /// Returns the raw-text state to switch to, if any.
    fn open(&mut self, name: String, attrs: Vec<(String, String)>, self_closing: bool) -> Option<RawKind> {
        // The spec ignores a second <form> while one is open.
        if name == "form" && self.stack.iter().any(|s| s == "form") {
            return None;
        }
        let closes = implies_close(&name);
        while self.stack.last().is_some_and(|top| closes.contains(&top.as_str())) {
            self.pop(true);
        }
        let void = is_void(&name);
        let was_foreign = self.in_foreign();
        if !void {
            self.stack.push(name.clone());
            if name == "svg" || name == "math" {
                self.contexts.push(Ctx::Foreign);
            } else if is_integration_point(&name) {
                self.contexts.push(Ctx::Html);
            }
        }
        (self.emit)(Event::Open { name: name.clone(), attrs });
        if void {
            (self.emit)(Event::Close { name, implied: true });
            return None;
        }
        // Self-closing syntax only counts in foreign content (e.g. `<circle/>`).
        if self_closing && (was_foreign || self.in_foreign()) {
            if self.stack.last() == Some(&name) {
                self.pop(false);
            }
            return None;
        }
        if was_foreign {
            return None;
        }
        match name.as_str() {
            // htmlparser2 ends <script> at the first `</script`; the spec's script-data escape
            // states would extend it (`<!--<script>` swallows the rest of the document). Script
            // content is never emitted, so matching htmlparser2 only keeps more following text.
            "script" | "style" | "xmp" | "iframe" | "noembed" | "noframes" => Some(RawKind::Rawtext),
            "title" | "textarea" => Some(RawKind::Rcdata),
            _ => None,
        }
    }

    fn close(&mut self, name: String) {
        if !is_void(&name) {
            if let Some(pos) = self.stack.iter().rposition(|s| *s == name) {
                let above = self.stack.len() - 1 - pos;
                for _ in 0..above {
                    self.pop(true);
                }
                self.pop(false);
            } else if name == "p" {
                // Implicit open before close.
                (self.emit)(Event::Open { name: "p".into(), attrs: Vec::new() });
                (self.emit)(Event::Close { name: "p".into(), implied: false });
            }
        } else if name == "br" {
            (self.emit)(Event::Open { name: "br".into(), attrs: Vec::new() });
            (self.emit)(Event::Close { name: "br".into(), implied: false });
        }
    }

    fn end(&mut self) {
        while !self.stack.is_empty() {
            self.pop(true);
        }
    }
}

struct Sink<F: FnMut(Event)> {
    tree: RefCell<Tree<F>>,
    /// Consecutive character tokens are delivered as one text event (htmlparser2 does).
    text: RefCell<String>,
}

impl<F: FnMut(Event)> Sink<F> {
    fn flush_text(&self, tree: &mut Tree<F>) {
        let mut text = self.text.borrow_mut();
        if !text.is_empty() {
            (tree.emit)(Event::Text(std::mem::take(&mut *text)));
        }
    }
}

impl<F: FnMut(Event)> TokenSink for Sink<F> {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut tree = self.tree.borrow_mut();
        if let Token::CharacterTokens(t) = &token {
            self.text.borrow_mut().push_str(t);
            return TokenSinkResult::Continue;
        }
        if matches!(token, Token::TagToken(_) | Token::EOFToken) {
            self.flush_text(&mut tree);
        }
        // Comments, doctypes, NULs and parse errors produce no events.
        if let Token::TagToken(tag) = token {
            match tag.kind {
                TagKind::StartTag => {
                    let name = tag.name.to_string();
                    let mut attrs: Vec<(String, String)> = Vec::with_capacity(tag.attrs.len());
                    for a in tag.attrs {
                        let key = a.name.local.to_string();
                        if !attrs.iter().any(|(k, _)| *k == key) {
                            attrs.push((key, a.value.to_string()));
                        }
                    }
                    let foreign = tree.in_foreign();
                    if name == "plaintext" && !foreign {
                        tree.open(name, attrs, false);
                        return TokenSinkResult::Plaintext;
                    }
                    if let Some(kind) = tree.open(name, attrs, tag.self_closing) {
                        return TokenSinkResult::RawData(kind);
                    }
                }
                TagKind::EndTag => tree.close(tag.name.to_string()),
            }
        }
        TokenSinkResult::Continue
    }
}

/// Parse `input`, calling `on` for every event. Linear in the input size.
pub fn parse(input: &str, on: impl FnMut(Event)) {
    let sink = Sink {
        tree: RefCell::new(Tree { stack: Vec::new(), contexts: Vec::new(), emit: on }),
        text: RefCell::new(String::new()),
    };
    let tokenizer = Tokenizer::new(sink, TokenizerOpts::default());
    let queue = BufferQueue::default();
    queue.push_back(StrTendril::from_slice(input));
    while let TokenizerResult::Script(()) = tokenizer.feed(&queue) {}
    tokenizer.end();
    let mut tree = tokenizer.sink.tree.borrow_mut();
    tokenizer.sink.flush_text(&mut tree);
    tree.end();
}

/// Collect events (for tests and small inputs).
pub fn events(input: &str) -> Vec<Event> {
    let mut out = Vec::new();
    parse(input, |e| out.push(e));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(input: &str) -> String {
        events(input)
            .into_iter()
            .map(|e| match e {
                Event::Open { name, .. } => format!("<{name}>"),
                Event::Close { name, implied: true } => format!("</{name}?>"),
                Event::Close { name, .. } => format!("</{name}>"),
                Event::Text(t) => t,
            })
            .collect()
    }

    #[test]
    fn implied_closes_follow_htmlparser2() {
        assert_eq!(trace("<p>a<p>b"), "<p>a</p?><p>b</p?>");
        assert_eq!(
            trace("<table><tr><td>a<td>b<tr><td>c</table>"),
            "<table><tr><td>a</td?><td>b</td?></tr?><tr><td>c</td?></tr?></table>"
        );
        assert_eq!(trace("<div><p>x</div>y"), "<div><p>x</p?></div>y");
        assert_eq!(trace("</p>z<br/>"), "<p></p>z<br></br?>");
        assert_eq!(trace("<b><i>m</b></i>"), "<b><i>m</i?></b>");
    }

    #[test]
    fn raw_text_only_outside_foreign_content() {
        assert_eq!(trace("<style>a<b>c</style>"), "<style>a<b>c</style>");
        assert_eq!(trace("<svg><style><b>x</b></style></svg>"), "<svg><style><b>x</b></style></svg>");
        assert_eq!(trace("<textarea>&amp;<b></textarea>"), "<textarea>&<b></textarea>");
        // Raw text arrives as one event even though the tokenizer splits it.
        assert_eq!(events("<iframe><img src=x></iframe>")[1], Event::Text("<img src=x>".into()));
    }
}
