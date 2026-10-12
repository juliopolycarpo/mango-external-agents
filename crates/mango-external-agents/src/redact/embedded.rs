//! A JSON text inside a string, read as the structure it is.
//!
//! A vendor that serialises a request into a field sends JSON as a string, and the structure in it
//! (a `{"name": ..., "value": ...}` pair, an `["Authorization", ...]` entry) is a credential the
//! text rules cannot see. This is the reader for that: a [`Layer`] over the text rules, which add
//! to what the other readers hide the values the structure says are credentials, and the strings
//! inside the text read once more as the text they decode to (a real line break where `\n` stood).
//!
//! It only adds. The text is read as it is written by every other reader first, the layer's
//! spans join theirs in the union, and what is hidden stays hidden. Then it makes sure the text
//! is still the JSON it was: a span that is not inside one string, or that cuts an escape in two,
//! is widened to the smallest value that holds it and that value is written as `"[REDACTED]"`.
//! What is not hidden is written as it was, byte for byte: a number, a key's order, a duplicate
//! key, the way a string is escaped.
//!
//! Only a text that parses as an object or an array is read, within [`MAX_BYTES`] and
//! [`MAX_DEPTH`] strings deep; past either it is the text rules' alone.

use std::ops::Range;

use super::structured::{Under, digits_make_a_count, only_digits};
use super::union::{Hide, Layer, union};
use super::{REDACTED, strip};

use self::spans::Node;

/// The longest string read as JSON: a larger one is a log, not a field, and the parse is the only
/// work here that is not a pass over the text. 256 KiB is a few thousand headers or variables.
pub(super) const MAX_BYTES: usize = 256 * 1024;

/// How many strings deep a JSON text is read, one inside the next.
pub(super) const MAX_DEPTH: usize = 4;

/// The marker as a JSON string.
const REDACTED_JSON: &str = "\"[REDACTED]\"";

/// A JSON text `depth` strings deep inside the value being redacted.
pub(super) struct Embedded {
    pub(super) depth: usize,
}

impl Layer for Embedded {
    fn layer(&self, plain: &str, floor: Vec<Hide>, others: Vec<Hide>) -> Vec<Hide> {
        let Some(tree) = self.parse(plain) else {
            return union(vec![floor, others]);
        };
        let others = clip(&tree, others);
        let mut mine = Vec::new();
        walk(&tree, Under::Nothing, self.depth, plain, &mut mine);
        shape(&tree, plain, union(vec![floor, others, mine]))
    }

    fn whole(&self, raw: &str) -> String {
        if self.parse(raw).is_some() {
            String::from(REDACTED_JSON)
        } else {
            String::from(REDACTED)
        }
    }
}

impl Embedded {
    /// The spans of `text`, when it is a JSON object or array within the bounds.
    fn parse(&self, text: &str) -> Option<spans::Spanned> {
        if self.depth >= MAX_DEPTH || text.len() > MAX_BYTES {
            return None;
        }
        let trimmed = text.trim();
        let wrapped = (trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']'));
        if !wrapped || serde_json::from_str::<serde::de::IgnoredAny>(text).is_err() {
            return None;
        }
        spans::parse(text)
    }
}

/// Pushes, in text order, the spans of the values of `node` that are credentials, each as the
/// string it becomes, and the spans of what the strings in it hide once decoded.
fn walk(node: &spans::Spanned, under: Under, depth: usize, text: &str, out: &mut Vec<Hide>) {
    let replace = |out: &mut Vec<Hide>| {
        out.push(Hide {
            start: node.span.start,
            end: node.span.end,
            lead: REDACTED_JSON.into(),
            tail: "".into(),
        });
    };
    match (&node.node, under) {
        // A container under a credential name is one marker, so that redacting never makes the
        // value larger. Except a list of counts under a name that counts tokens.
        (Node::Array(items), Under::Credential { counts: true })
            if items.iter().all(|item| is_a_count(item, text)) => {}
        (Node::Object(_) | Node::Array(_), Under::Credential { .. }) => replace(out),
        (Node::Object(members), _) => {
            let names = |keys: &[&str]| {
                members
                    .iter()
                    .find(|member| keys.iter().any(|key| member.key.eq_ignore_ascii_case(key)))
            };
            // `{"name": "Authorization", "value": "Bearer ..."}`: the value is the credential.
            let pair_value = names(&["name", "key", "header", "field"])
                .and_then(|member| match &member.value.node {
                    Node::String(name) => Under::credential(name),
                    _ => None,
                })
                .and_then(|_| names(&["value", "val"]))
                .map(|member| member.key.as_str());
            for member in members {
                let below = if pair_value == Some(member.key.as_str()) {
                    Under::Credential { counts: false }
                } else {
                    Under::credential(&member.key).unwrap_or(Under::Nothing)
                };
                walk(&member.value, below, depth, text, out);
            }
        }
        (Node::Array(items), under) => {
            // `["Authorization", "Bearer ..."]`: the second entry is the credential.
            let pair = matches!(items.as_slice(),
                [first, second]
                    if matches!(&first.node, Node::String(name) if Under::credential(name).is_some())
                        && matches!(second.node, Node::String(_)))
                && matches!(under, Under::Nothing);
            for (index, item) in items.iter().enumerate() {
                let below = if pair && index == 1 {
                    Under::Credential { counts: false }
                } else {
                    under
                };
                walk(item, below, depth, text, out);
            }
        }
        (Node::String(decoded), Under::Credential { counts }) => {
            if !(counts && only_digits(decoded) && digits_make_a_count(decoded)) {
                replace(out);
            }
        }
        (Node::String(decoded), Under::Nothing) => {
            hide_in_string(node, decoded, depth, text, out);
        }
        (Node::Number, Under::Credential { counts: false }) => replace(out),
        (Node::Number, Under::Credential { counts: true }) => {
            if !is_a_count(node, text) {
                replace(out);
            }
        }
        (Node::Number | Node::Other, _) => {}
    }
}

/// Whether the node is a number written with few enough digits to be a count.
fn is_a_count(node: &spans::Spanned, text: &str) -> bool {
    matches!(node.node, Node::Number) && digits_make_a_count(&text[node.span.clone()])
}

/// The spans a string hides when it is read as the text it decodes to, as spans of where it is
/// written: everything between its quotes that the readers hide in the decoded text, and nothing
/// of what they leave. A string that decodes to something the stripper changes cannot be mapped
/// back, and is written again whole (the text rules strip the same bytes from any text they read).
fn hide_in_string(
    node: &spans::Spanned,
    decoded: &str,
    depth: usize,
    text: &str,
    out: &mut Vec<Hide>,
) {
    let (plain, marked) = strip::strip_marking(decoded);
    let sub = Embedded { depth: depth + 1 };
    let found = super::union::hides(&plain, marked, &sub);
    if found.is_empty() {
        return;
    }
    let content = node.span.start + 1..node.span.end - 1;
    let Some(map) = Decoded::new(&text[content.clone()]) else {
        return;
    };
    if plain != decoded {
        let shown = super::union::finish(&plain, &found);
        out.push(Hide {
            start: node.span.start,
            end: node.span.end,
            lead: json_string(&shown).into(),
            tail: "".into(),
        });
        return;
    }
    for hide in found {
        let (Some(start), Some(end)) = (map.written(hide.start), map.written(hide.end)) else {
            continue;
        };
        out.push(Hide {
            start: content.start + start,
            end: content.start + end,
            lead: escaped(&hide.lead).into(),
            tail: escaped(&hide.tail).into(),
        });
    }
}

/// `text` as a JSON string, quotes included.
fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| String::from(REDACTED_JSON))
}

/// `text` as the inside of a JSON string.
fn escaped(text: &str) -> String {
    let quoted = json_string(text);
    quoted[1..quoted.len() - 1].to_owned()
}

/// A string's content as it decodes, and where each of its bytes is written.
struct Decoded {
    /// For each place a byte of the decoded string starts, where the escape or the character that
    /// wrote it starts in the content; the content's length stands for the end. A place in the
    /// middle of an escape or of a multi-byte character has no entry.
    starts: Vec<(usize, usize)>,
}

impl Decoded {
    fn new(content: &str) -> Option<Self> {
        let bytes = content.as_bytes();
        let mut starts = Vec::with_capacity(content.len() + 1);
        let (mut at, mut decoded) = (0, 0);
        while at < bytes.len() {
            starts.push((decoded, at));
            let (character, width) = if bytes[at] == b'\\' {
                escape(bytes, at)?
            } else {
                let character = content[at..].chars().next()?;
                (character, character.len_utf8())
            };
            decoded += character.len_utf8();
            at += width;
        }
        starts.push((decoded, content.len()));
        Some(Self { starts })
    }

    /// Where the decoded byte `decoded` is written, when a character or an escape starts there.
    fn written(&self, decoded: usize) -> Option<usize> {
        let at = self
            .starts
            .binary_search_by_key(&decoded, |(place, _)| *place)
            .ok()?;
        Some(self.starts[at].1)
    }

    /// Whether a character or an escape starts at the written place `at` of the content.
    fn starts_at(&self, at: usize) -> bool {
        self.starts
            .binary_search_by_key(&at, |(_, written)| *written)
            .is_ok()
    }
}

/// The character the escape at `at` writes, and how many bytes it takes.
fn escape(bytes: &[u8], at: usize) -> Option<(char, usize)> {
    let simple = |character| Some((character, 2));
    match *bytes.get(at + 1)? {
        b'"' => simple('"'),
        b'\\' => simple('\\'),
        b'/' => simple('/'),
        b'b' => simple('\u{8}'),
        b'f' => simple('\u{c}'),
        b'n' => simple('\n'),
        b'r' => simple('\r'),
        b't' => simple('\t'),
        b'u' => {
            let high = hex4(bytes, at + 2)?;
            if (0xd800..0xdc00).contains(&high) {
                (bytes.get(at + 6) == Some(&b'\\') && bytes.get(at + 7) == Some(&b'u'))
                    .then_some(())?;
                let low = hex4(bytes, at + 8)?;
                let point = 0x10000 + ((high - 0xd800) << 10) + low.checked_sub(0xdc00)?;
                Some((char::from_u32(point)?, 12))
            } else {
                Some((char::from_u32(high)?, 6))
            }
        }
        _ => None,
    }
}

fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let digits = std::str::from_utf8(bytes.get(at..at + 4)?).ok()?;
    u32::from_str_radix(digits, 16).ok()
}

/// The spans of `others`, the reads added since the release before, with a span that starts inside
/// a string and runs past its end cut at the closing quote.
///
/// Such a span is a rule reading JSON punctuation as part of a value (`token=- A\n- B","password":
/// "p"}` is a sequence item to the end of the line). What it hides inside the string, the string
/// read as the text it decodes to hides as well, with the end of the string as the end of the text;
/// what is past the string is structure, which the walk reads. The floor is never cut: what the
/// release before hid is widened to values by [`shape`] and stays hidden.
fn clip(tree: &spans::Spanned, others: Vec<Hide>) -> Vec<Hide> {
    others
        .into_iter()
        .filter_map(|hide| {
            let (span, is_a_string) = innermost(tree, &(hide.start..hide.start.saturating_add(1)));
            let closing = span.end.saturating_sub(1);
            if !is_a_string || hide.start <= span.start || hide.end <= closing {
                return Some(hide);
            }
            (hide.start < closing).then(|| Hide {
                end: closing,
                tail: "".into(),
                ..hide
            })
        })
        .collect()
}

/// `hides`, made to leave `text`, which parses as `tree`, a JSON text: a span that lies inside one
/// string, on the edges of its characters and escapes, stays; any other span becomes the values and
/// keys it touches, each written as `"[REDACTED]"`, and a value it holds whole becomes one.
fn shape(tree: &spans::Spanned, text: &str, hides: Vec<Hide>) -> Vec<Hide> {
    let mut shaped = Vec::with_capacity(hides.len());
    let mut edges = Edges::default();
    for hide in hides {
        match kept_in_a_string(tree, text, &hide, &mut edges) {
            Some(kept) => shaped.push(kept),
            None => cover(tree, &(hide.start..hide.end), &mut shaped),
        }
    }
    shaped.sort_by_key(|hide| (hide.start, hide.end));
    union(vec![shaped])
}

/// `hide`, written for the inside of a JSON string, when it lies in one, on the edges of its
/// characters and escapes.
fn kept_in_a_string(
    tree: &spans::Spanned,
    text: &str,
    hide: &Hide,
    edges: &mut Edges,
) -> Option<Hide> {
    let range = hide.start..hide.end;
    let (span, is_a_string) = innermost(tree, &range);
    (is_a_string && inside_the_quotes(&span, &range) && edges.on_the_edges(text, &span, &range))
        .then(|| Hide {
            start: hide.start,
            end: hide.end,
            lead: escaped(&hide.lead).into(),
            tail: escaped(&hide.tail).into(),
        })
}

/// The marker as a span of `span`'s text.
fn whole(span: &Range<usize>) -> Hide {
    Hide {
        start: span.start,
        end: span.end,
        lead: REDACTED_JSON.into(),
        tail: "".into(),
    }
}

/// Pushes the spans that hide `range` in `node` and keep it JSON: a node `range` holds whole, or
/// a string, number or key it touches.
fn cover(node: &spans::Spanned, range: &Range<usize>, out: &mut Vec<Hide>) {
    let span = &node.span;
    if !(range.start < span.end && span.start < range.end) {
        return;
    }
    if range.start <= span.start && span.end <= range.end {
        out.push(whole(span));
        return;
    }
    match &node.node {
        Node::Object(members) => {
            let first = members.partition_point(|member| member.value.span.end <= range.start);
            for member in &members[first..] {
                if member.key_span.start >= range.end {
                    break;
                }
                if range.start < member.key_span.end {
                    out.push(whole(&member.key_span));
                }
                cover(&member.value, range, out);
            }
        }
        Node::Array(items) => {
            let first = items.partition_point(|item| item.span.end <= range.start);
            for item in &items[first..] {
                if item.span.start >= range.end {
                    break;
                }
                cover(item, range, out);
            }
        }
        _ => out.push(whole(span)),
    }
}

/// The span of the smallest value, or key, that holds `range`, and whether it is a string.
fn innermost(node: &spans::Spanned, range: &Range<usize>) -> (Range<usize>, bool) {
    let mut found = (node.span.clone(), matches!(node.node, Node::String(_)));
    match &node.node {
        Node::Object(members) => {
            let at = members.partition_point(|member| member.value.span.end < range.end);
            if let Some(member) = members.get(at) {
                if contains(&member.key_span, range) {
                    return (member.key_span.clone(), true);
                }
                if contains(&member.value.span, range) {
                    found = innermost(&member.value, range);
                }
            }
        }
        Node::Array(items) => {
            let at = items.partition_point(|item| item.span.end < range.end);
            if let Some(item) = items.get(at).filter(|item| contains(&item.span, range)) {
                found = innermost(item, range);
            }
        }
        _ => {}
    }
    found
}

fn contains(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// Whether `range` is between the quotes of the string written at `span`.
fn inside_the_quotes(span: &Range<usize>, range: &Range<usize>) -> bool {
    span.start < range.start && range.end < span.end
}

/// Where the characters and escapes of the string last asked about start: one string holds many
/// hides, and the places are found once for all of them.
#[derive(Default)]
struct Edges {
    string: Option<(Range<usize>, Option<Decoded>)>,
}

impl Edges {
    /// Whether both ends of `range` stand where a character or an escape of the string written at
    /// `span` starts.
    fn on_the_edges(&mut self, text: &str, span: &Range<usize>, range: &Range<usize>) -> bool {
        if self.string.as_ref().is_none_or(|(known, _)| known != span) {
            let content = &text[span.start + 1..span.end - 1];
            self.string = Some((span.clone(), Decoded::new(content)));
        }
        let Some((_, Some(map))) = &self.string else {
            return false;
        };
        let base = span.start + 1;
        map.starts_at(range.start - base) && map.starts_at(range.end - base)
    }
}

/// A scan of text that is JSON already: the spans of its values, and the strings it holds decoded.
mod spans {
    use std::ops::Range;

    /// A value and where it is written.
    pub(super) struct Spanned {
        pub(super) span: Range<usize>,
        pub(super) node: Node,
    }

    pub(super) enum Node {
        Object(Vec<Member>),
        Array(Vec<Spanned>),
        /// A string, decoded.
        String(String),
        Number,
        /// `true`, `false` and `null`.
        Other,
    }

    pub(super) struct Member {
        pub(super) key: String,
        /// Where the key is written, quotes included.
        pub(super) key_span: Range<usize>,
        pub(super) value: Spanned,
    }

    /// The value `text` holds, or `None` when it is not the JSON it was taken for.
    pub(super) fn parse(text: &str) -> Option<Spanned> {
        let mut scan = Scan {
            text,
            bytes: text.as_bytes(),
            at: 0,
        };
        let value = scan.value()?;
        scan.blanks();
        (scan.at == text.len()).then_some(value)
    }

    struct Scan<'a> {
        text: &'a str,
        bytes: &'a [u8],
        at: usize,
    }

    impl Scan<'_> {
        fn blanks(&mut self) {
            while self
                .bytes
                .get(self.at)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
            {
                self.at += 1;
            }
        }

        fn value(&mut self) -> Option<Spanned> {
            self.blanks();
            let start = self.at;
            let node = match *self.bytes.get(start)? {
                b'{' => self.object()?,
                b'[' => self.array()?,
                b'"' => Node::String(self.string()?),
                b'-' | b'0'..=b'9' => {
                    while self.bytes.get(self.at).is_some_and(|byte| {
                        matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                    }) {
                        self.at += 1;
                    }
                    Node::Number
                }
                _ => {
                    while self.bytes.get(self.at).is_some_and(u8::is_ascii_alphabetic) {
                        self.at += 1;
                    }
                    Node::Other
                }
            };
            Some(Spanned {
                span: start..self.at,
                node,
            })
        }

        fn string(&mut self) -> Option<String> {
            let start = self.at;
            self.at += 1;
            while let Some(&byte) = self.bytes.get(self.at) {
                self.at += 1;
                match byte {
                    b'\\' => self.at += 1,
                    b'"' => return serde_json::from_str(&self.text[start..self.at]).ok(),
                    _ => {}
                }
            }
            None
        }

        fn object(&mut self) -> Option<Node> {
            self.at += 1;
            let mut members = Vec::new();
            loop {
                self.blanks();
                match self.bytes.get(self.at)? {
                    b'}' => {
                        self.at += 1;
                        return Some(Node::Object(members));
                    }
                    b',' => self.at += 1,
                    b'"' => {
                        let key_start = self.at;
                        let key = self.string()?;
                        let key_span = key_start..self.at;
                        self.blanks();
                        (self.bytes.get(self.at) == Some(&b':')).then_some(())?;
                        self.at += 1;
                        let value = self.value()?;
                        members.push(Member {
                            key,
                            key_span,
                            value,
                        });
                    }
                    _ => return None,
                }
            }
        }

        fn array(&mut self) -> Option<Node> {
            self.at += 1;
            let mut items = Vec::new();
            loop {
                self.blanks();
                match self.bytes.get(self.at)? {
                    b']' => {
                        self.at += 1;
                        return Some(Node::Array(items));
                    }
                    b',' => self.at += 1,
                    _ => items.push(self.value()?),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoded, MAX_BYTES};
    use crate::redact::structured;
    use serde_json::Value;

    /// What a string that holds the JSON text `text` comes back as under the walk.
    fn read(text: &str) -> String {
        match structured(&Value::String(text.to_owned())) {
            Value::String(shown) => shown,
            other => panic!("expected a string back for {text:?} | received: {other}"),
        }
    }

    fn assert_read(cases: &[(&str, &str)]) {
        for (text, expected) in cases {
            let received = read(text);
            assert_eq!(
                &received, expected,
                "expected {text:?} to be read as {expected:?} | received: {received:?}"
            );
            assert!(
                serde_json::from_str::<Value>(&received).is_ok(),
                "expected the answer for {text:?} to stay JSON | received: {received:?}"
            );
        }
    }

    #[test]
    fn what_the_structure_says_is_a_credential_is_hidden_and_the_rest_is_written_as_it_was() {
        assert_read(&[
            // A key's order, a number a parser would round, an escape and a duplicate key stay.
            (
                r#"{"z":1,"password":"p","a":100000000000000000000000000001,"z":2}"#,
                r#"{"z":1,"password":"[REDACTED]","a":100000000000000000000000000001,"z":2}"#,
            ),
            (
                r#"{"name":"Authorization","value":"Bearer x1","n":1.50}"#,
                r#"{"name":"Authorization","value":"[REDACTED]","n":1.50}"#,
            ),
            (
                r#"[["Authorization","Bearer x1"],["Accept","a"]]"#,
                r#"[["Authorization","[REDACTED]"],["Accept","a"]]"#,
            ),
            (
                // The text rules hide a list under `tokens` as they always did, since a list is
                // not a count.
                r#"{"tokens":[1,2,3],"credentials":{"user":"u"},"secrets":["a"]}"#,
                r#"{"tokens":"[REDACTED]","credentials":"[REDACTED]","secrets":"[REDACTED]"}"#,
            ),
        ]);
    }

    #[test]
    fn a_string_that_holds_a_credential_keeps_the_bytes_outside_it() {
        assert_read(&[
            (
                "{\"note\":\"\u{e9} token=QXZ1J \u{e9}\",\"n\":1}",
                "{\"note\":\"\u{e9} token=[REDACTED] \u{e9}\",\"n\":1}",
            ),
            (
                r#"{"note":"\u00e9 token=QXZ1J \ud83d\ude00","n":1}"#,
                r#"{"note":"\u00e9 token=[REDACTED] \ud83d\ude00","n":1}"#,
            ),
            (
                r#"{"note":"a\nb token=QXZ1J c","n":1}"#,
                r#"{"note":"a\nb token=[REDACTED] c","n":1}"#,
            ),
        ]);
    }

    #[test]
    fn a_hide_that_would_break_the_json_is_widened_to_the_value_that_holds_it() {
        assert_read(&[
            // A sequence item in a string stops at the string. The marker left in front of the
            // closing quote is read as a list by the rules of the release before, which take the
            // quote with it, so the second reading hides the whole string.
            (
                r#"{"m":"token=- A\n- B","password":"p"}"#,
                r#"{"m":"[REDACTED]","password":"[REDACTED]"}"#,
            ),
            // A key that is a token is written as a hidden key.
            (
                &format!(
                    r#"{{"{}{}":1,"n":2}}"#,
                    concat!("gh", "p_"),
                    "a1B2c3D4e5F6g7H8i9J0a1B2c3D4e5F6g7H8"
                ),
                r#"{"[REDACTED]":1,"n":2}"#,
            ),
        ]);
    }

    #[test]
    fn a_text_that_is_no_object_or_array_or_is_too_large_is_left_to_the_text_rules() {
        for text in ["123", "\"token=QXZ1J\"", "null", "{\"a\":"] {
            let received = read(text);
            assert!(
                !received.contains("QXZ1J"),
                "expected {text:?} read by the text rules | received: {received:?}"
            );
        }
        let large = format!(
            "{{\"a\":\"{}\",\"password\":\"QXZ1J\"}}",
            "x".repeat(MAX_BYTES)
        );
        assert!(
            !read(&large).contains("QXZ1J"),
            "expected a text past the bound to be read by the text rules, and hide the password"
        );
    }

    #[test]
    fn the_places_a_string_is_written_at_are_found_for_escapes_and_characters() {
        let map = Decoded::new(r"a\n\u00e9\ud83d\ude00z").expect("a valid string");
        // `a`, the newline, `é` (two bytes), the emoji (four bytes) and `z`.
        for (decoded, written) in [(0, 0), (1, 1), (2, 3), (4, 9), (8, 21)] {
            assert_eq!(
                map.written(decoded),
                Some(written),
                "expected decoded byte {decoded} written at {written}"
            );
        }
        assert_eq!(map.written(3), None, "expected no place in the middle of é");
        assert!(
            Decoded::new(r"\q").is_none(),
            "expected an unknown escape refused"
        );
        assert!(
            Decoded::new(r"\ud83d").is_none(),
            "expected a lone surrogate refused"
        );
    }
}
