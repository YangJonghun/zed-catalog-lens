//! serde_json rejects a document mid-edit, so this module scans the text on
//! its own. Every position is reported in UTF-16 code units, the LSP default.

use std::cell::Cell;
use std::collections::HashSet;

use tower_lsp_server::ls_types::{Position, Range};

pub const CATALOG_PREFIX: &str = "catalog:";

const DEPENDENCY_FIELDS: [&str; 6] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "overrides",
    "resolutions",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub key: String,
    pub package: String,
    pub raw_catalog: String,
    pub range: Range,
    pub entry: Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSlot {
    pub package: String,
    pub typed: String,
    pub content_range: Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Str {
    pub value: String,
    pub start: usize,
    pub end: usize,
    pub content_start: usize,
    pub content_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Object(Vec<(Str, Node)>),
    Array(Vec<Node>),
    Str(Str),
    Other,
}

impl Node {
    pub fn field(&self, name: &str) -> Option<&Node> {
        match self {
            Node::Object(fields) => fields
                .iter()
                .find(|(key, _)| key.value == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn fields(&self) -> &[(Str, Node)] {
        match self {
            Node::Object(fields) => fields,
            _ => &[],
        }
    }
}

pub const BYTE_ORDER_MARK: char = '\u{feff}';

/// Containers nested deeper than this are skipped, not parsed: recursing
/// into them overflowed a 2 MiB tokio worker stack near 6000 levels.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duplicates {
    /// `JSON.parse`, which pnpm and Yarn read package.json with: the last wins.
    Last,
    /// Bun keeps every property, and its lookups take the first.
    All,
}

pub fn parse(text: &str, duplicates: Duplicates) -> Node {
    let mut scanner = Scanner {
        text,
        src: text.as_bytes(),
        depth: 0,
        duplicates,
        pos: if text.starts_with(BYTE_ORDER_MARK) {
            BYTE_ORDER_MARK.len_utf8()
        } else {
            0
        },
    };
    scanner.skip_trivia();
    scanner.value().unwrap_or(Node::Other)
}

struct Scanner<'a> {
    text: &'a str,
    src: &'a [u8],
    pos: usize,
    depth: usize,
    duplicates: Duplicates,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.pos += 1,
                Some(b'/') if self.src.get(self.pos + 1) == Some(&b'/') => {
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.src.get(self.pos + 1) == Some(&b'*') => {
                    self.pos += 2;
                    while self.peek().is_some()
                        && !(self.peek() == Some(b'*') && self.src.get(self.pos + 1) == Some(&b'/'))
                    {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.src.len());
                }
                _ => return,
            }
        }
    }

    fn value(&mut self) -> Option<Node> {
        match self.peek()? {
            b'{' | b'[' if self.depth >= MAX_DEPTH => {
                self.skip_container();
                Some(Node::Other)
            }
            b'{' | b'[' => {
                self.depth += 1;
                let node = if self.peek() == Some(b'{') {
                    self.object()
                } else {
                    self.array()
                };
                self.depth -= 1;
                Some(node)
            }
            b'"' => Some(Node::Str(self.string())),
            b'}' | b']' | b',' | b':' => None,
            _ => {
                while !matches!(
                    self.peek(),
                    None | Some(b',' | b'}' | b']' | b':' | b' ' | b'\t' | b'\r' | b'\n')
                ) {
                    self.pos += 1;
                }
                Some(Node::Other)
            }
        }
    }

    fn skip_container(&mut self) {
        let mut open = 0usize;
        while let Some(byte) = self.peek() {
            match byte {
                b'{' | b'[' => {
                    open += 1;
                    self.pos += 1;
                }
                b'}' | b']' => {
                    self.pos += 1;
                    open -= 1;
                    if open == 0 {
                        return;
                    }
                }
                b'"' => {
                    self.string();
                }
                _ => self.pos += self.next_char().map_or(1, char::len_utf8),
            }
        }
    }

    fn next_char(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn object(&mut self) -> Node {
        self.pos += 1;
        let mut fields = Vec::new();
        let mut pending_key: Option<Str> = None;
        loop {
            self.skip_trivia();
            match self.peek() {
                None => break,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') | Some(b':') => self.pos += 1,
                Some(b'"') => {
                    let string = self.string();
                    self.skip_trivia();
                    match pending_key.take() {
                        Some(key) if self.peek() != Some(b':') => {
                            fields.push((key, Node::Str(string)));
                        }
                        previous => {
                            if let Some(key) = previous {
                                fields.push((key, Node::Other));
                            }
                            pending_key = Some(string);
                        }
                    }
                }
                Some(_) => match pending_key.take() {
                    Some(key) => {
                        let value = self.value().unwrap_or(Node::Other);
                        fields.push((key, value));
                    }
                    None => {
                        if self.value().is_none() {
                            self.pos += 1;
                        }
                    }
                },
            }
        }
        if let Some(key) = pending_key {
            fields.push((key, Node::Other));
        }
        if self.duplicates == Duplicates::Last {
            keep_last(&mut fields);
        }
        Node::Object(fields)
    }

    fn array(&mut self) -> Node {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => break,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => self.pos += 1,
                Some(_) => match self.value() {
                    Some(node) => items.push(node),
                    None => self.pos += 1,
                },
            }
        }
        Node::Array(items)
    }

    fn string(&mut self) -> Str {
        let start = self.pos;
        self.pos += 1;
        let content_start = self.pos;
        let mut value = String::new();
        let (content_end, end) = loop {
            match self.peek() {
                None => break (self.pos, self.pos),
                Some(b'\n') | Some(b'\r') => break (self.pos, self.pos),
                Some(b'"') => break (self.pos, self.pos + 1),
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut value);
                }
                Some(_) => {
                    let ch = self.next_char().unwrap_or_default();
                    value.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        };
        self.pos = end;
        Str {
            value,
            start,
            end,
            content_start,
            content_end,
        }
    }

    fn escape(&mut self, out: &mut String) {
        let Some(escaped) = self.next_char() else {
            return;
        };
        self.pos += escaped.len_utf8();
        match escaped {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'u' => {
                let high = self.hex4();
                let code = match high {
                    Some(h @ 0xD800..=0xDBFF)
                        if self.src.get(self.pos) == Some(&b'\\')
                            && self.src.get(self.pos + 1) == Some(&b'u') =>
                    {
                        let saved = self.pos;
                        self.pos += 2;
                        match self.hex4() {
                            Some(l @ 0xDC00..=0xDFFF) => {
                                Some(0x10000 + ((h - 0xD800) << 10) + (l - 0xDC00))
                            }
                            _ => {
                                self.pos = saved;
                                Some(h)
                            }
                        }
                    }
                    other => other,
                };
                out.push(code.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
            }
            other => out.push(other),
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.src.get(self.pos..self.pos + 4)?;
        let text = std::str::from_utf8(digits).ok()?;
        let code = u32::from_str_radix(text, 16).ok()?;
        self.pos += 4;
        Some(code)
    }
}

fn keep_last(fields: &mut Vec<(Str, Node)>) {
    if fields.len() < 2 {
        return;
    }
    let mut later = HashSet::with_capacity(fields.len());
    let mut last = vec![false; fields.len()];
    for (i, (key, _)) in fields.iter().enumerate().rev() {
        last[i] = later.insert(key.value.as_str());
    }
    let mut last = last.into_iter();
    fields.retain(|_| last.next().unwrap_or(true));
}

pub struct LineIndex<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
    cursor: Cell<Point>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Point {
    line: usize,
    byte: usize,
    utf16: usize,
    chars: usize,
}

impl Point {
    fn advance(&mut self, ch: char) {
        self.byte += ch.len_utf8();
        self.utf16 += ch.len_utf16();
        self.chars += 1;
    }
}

impl<'a> LineIndex<'a> {
    /// Lines break at `\r\n`, a lone `\r`, and `\n`, as in YAML and the LSP.
    pub fn new(text: &'a str) -> Self {
        let bytes = text.as_bytes();
        let mut line_starts = vec![0];
        line_starts.extend(bytes.iter().enumerate().filter_map(|(i, &byte)| {
            let breaks = byte == b'\n' || (byte == b'\r' && bytes.get(i + 1) != Some(&b'\n'));
            breaks.then_some(i + 1)
        }));
        Self {
            text,
            line_starts,
            cursor: Cell::default(),
        }
    }

    fn line_end(&self, line: usize) -> usize {
        match self.line_starts.get(line + 1) {
            Some(&next) if self.text[..next].ends_with("\r\n") => next - 2,
            Some(&next) => next - 1,
            None => self.text.len(),
        }
    }

    fn walk_from(&self, line: usize, reachable: impl Fn(&Point) -> bool) -> Point {
        let cursor = self.cursor.get();
        if cursor.line == line && reachable(&cursor) {
            cursor
        } else {
            Point {
                line,
                byte: self.line_starts[line],
                ..Point::default()
            }
        }
    }

    pub fn position(&self, offset: usize) -> Position {
        let offset = offset.min(self.text.len());
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        let offset = offset.min(self.line_end(line));
        let mut point = self.walk_from(line, |point| point.byte <= offset);
        for ch in self.text[point.byte..offset].chars() {
            point.advance(ch);
        }
        self.cursor.set(point);
        Position::new(line as u32, point.utf16 as u32)
    }

    pub fn offset_at_char(&self, line: usize, column: usize) -> Option<usize> {
        self.line_starts.get(line)?;
        let line_end = self.line_end(line);
        let mut point = self.walk_from(line, |point| point.chars <= column);
        for ch in self.text[point.byte..line_end].chars() {
            if point.chars == column {
                break;
            }
            point.advance(ch);
        }
        self.cursor.set(point);
        Some(point.byte)
    }

    pub fn range(&self, start: usize, end: usize) -> Range {
        Range::new(self.position(start), self.position(end))
    }

    pub fn offset(&self, position: Position) -> Option<usize> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        let line_end = self.line_end(position.line as usize);
        let line = &self.text[line_start..line_end];
        let mut units = 0;
        for (index, ch) in line.char_indices() {
            if units >= position.character as usize {
                return Some(line_start + index);
            }
            units += ch.len_utf16();
        }
        Some(line_end)
    }
}

pub fn references(text: &str) -> Vec<Reference> {
    let index = LineIndex::new(text);
    let root = parse(text, Duplicates::Last);
    let mut out = Vec::new();
    for (key, value) in root.fields() {
        if DEPENDENCY_FIELDS.contains(&key.value.as_str()) {
            collect(value, None, &index, &mut out);
        }
    }
    if let Some(overrides) = root.field("pnpm").and_then(|pnpm| pnpm.field("overrides")) {
        collect(overrides, None, &index, &mut out);
    }
    out
}

/// In npm overrides, `"."` inside `"foo": { ... }` sets the version of `foo`
/// itself.
fn target_package(key: &str, parent: Option<&str>) -> String {
    match parent {
        Some(parent) if key == "." => parent.to_owned(),
        _ => normalize_selector(key),
    }
}

fn collect(node: &Node, parent: Option<&str>, index: &LineIndex, out: &mut Vec<Reference>) {
    for (key, value) in node.fields() {
        let package = target_package(&key.value, parent);
        match value {
            Node::Str(string) => {
                if let Some(raw_catalog) = string.value.strip_prefix(CATALOG_PREFIX) {
                    out.push(Reference {
                        key: key.value.clone(),
                        package,
                        raw_catalog: raw_catalog.to_owned(),
                        range: index.range(string.start, string.end),
                        entry: index.range(key.start, string.end),
                    });
                }
            }
            Node::Object(_) => collect(value, Some(&package), index, out),
            _ => {}
        }
    }
}

pub fn reference_at(
    references: &[Reference],
    position: Position,
    span: fn(&Reference) -> Range,
) -> Option<&Reference> {
    references.iter().find(|reference| {
        let range = span(reference);
        range.start <= position && position <= range.end
    })
}

pub fn normalize_selector(key: &str) -> String {
    let segment = key[last_selector_separator(key)..].trim();
    let mut parts = segment
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.chars().all(|c| c == '*'))
        .peekable();
    let mut last = String::new();
    while let Some(part) = parts.next() {
        if part.starts_with('@') {
            match parts.next() {
                Some(name) => last = format!("{part}/{name}"),
                None => last = part.to_owned(),
            }
        } else {
            last = part.to_owned();
        }
    }
    strip_version(&last).to_owned()
}

fn last_selector_separator(key: &str) -> usize {
    let bytes = key.as_bytes();
    (1..bytes.len().saturating_sub(1))
        .rev()
        .find(|&i| {
            bytes[i] == b'>'
                && !b"@<>=|~^ ".contains(&bytes[i - 1])
                && !b"= ".contains(&bytes[i + 1])
        })
        .map_or(0, |i| i + 1)
}

fn strip_version(name: &str) -> &str {
    let search_from = usize::from(name.starts_with('@'));
    match name[search_from..].find('@') {
        Some(at) => &name[..search_from + at],
        None => name,
    }
}

pub fn value_slot_at(text: &str, position: Position) -> Option<ValueSlot> {
    let index = LineIndex::new(text);
    let offset = index.offset(position)?;
    let root = parse(text, Duplicates::Last);
    let mut slot = None;
    for (key, value) in root.fields() {
        if DEPENDENCY_FIELDS.contains(&key.value.as_str()) {
            find_slot(value, None, offset, &index, &mut slot);
        }
    }
    if let Some(overrides) = root.field("pnpm").and_then(|pnpm| pnpm.field("overrides")) {
        find_slot(overrides, None, offset, &index, &mut slot);
    }
    slot
}

fn find_slot(
    node: &Node,
    parent: Option<&str>,
    offset: usize,
    index: &LineIndex,
    slot: &mut Option<ValueSlot>,
) {
    for (key, value) in node.fields() {
        let package = target_package(&key.value, parent);
        match value {
            Node::Str(string) if string.content_start <= offset && offset <= string.content_end => {
                *slot = Some(ValueSlot {
                    package,
                    typed: index.text[string.content_start..offset].to_owned(),
                    content_range: index.range(string.content_start, string.content_end),
                });
                return;
            }
            Node::Object(_) => find_slot(value, Some(&package), offset, index, slot),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(text: &str) -> Vec<(String, String, String)> {
        references(text)
            .into_iter()
            .map(|r| (r.key, r.package, r.raw_catalog))
            .collect()
    }

    fn pair(key: &str, package: &str, catalog: &str) -> (String, String, String) {
        (key.to_owned(), package.to_owned(), catalog.to_owned())
    }

    #[test]
    fn extracts_references_from_every_dependency_field() {
        let text = r#"{
          "dependencies": { "react": "catalog:" },
          "devDependencies": { "vitest": "catalog:test" },
          "peerDependencies": { "react-dom": "catalog:react18" },
          "optionalDependencies": { "fsevents": "catalog:" },
          "resolutions": { "lodash": "catalog:" },
          "overrides": { "foo": { "bar": "catalog:nested" } },
          "pnpm": { "overrides": { "foo>react@^17": "catalog:react17" } }
        }"#;
        assert_eq!(
            refs(text),
            vec![
                pair("react", "react", ""),
                pair("vitest", "vitest", "test"),
                pair("react-dom", "react-dom", "react18"),
                pair("fsevents", "fsevents", ""),
                pair("lodash", "lodash", ""),
                pair("bar", "bar", "nested"),
                pair("foo>react@^17", "react", "react17"),
            ]
        );
    }

    #[test]
    fn ignores_values_without_the_colon_and_other_fields() {
        let text = r#"{ "name": "catalog:", "dependencies": { "a": "catalog", "b": "^1.0.0" } }"#;
        assert!(refs(text).is_empty());
    }

    #[test]
    fn survives_unclosed_string_and_object() {
        let text =
            "{\n  \"dependencies\": {\n    \"react\": \"catalog:\",\n    \"vue\": \"catalog:v\n";
        let found = references(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].raw_catalog, "v");
        assert_eq!(found[1].range.end, Position::new(3, 21));
    }

    #[test]
    fn survives_trailing_commas_comments_and_crlf() {
        let text = "{\r\n  // comment\r\n  \"dependencies\": {\r\n    /* block */ \"react\": \"catalog:\",\r\n  },\r\n}\r\n";
        let found = references(text);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].range,
            Range::new(Position::new(3, 25), Position::new(3, 35))
        );
    }

    #[test]
    fn positions_are_utf16_code_units() {
        let text =
            "{ \"description\": \"🔭 한글\", \"dependencies\": { \"react\": \"catalog:\" } }";
        let found = references(text);
        let start = text.find("\"catalog:\"").unwrap();
        let expected = text[..start].encode_utf16().count() as u32;
        assert_eq!(found[0].range.start, Position::new(0, expected));
        assert_eq!(found[0].range.end, Position::new(0, expected + 10));
    }

    #[test]
    fn decodes_unicode_escapes_in_keys() {
        let text = r#"{ "dependencies": { "\u0040scope/pkg": "catalog:" } }"#;
        assert!(text.contains('\\'));
        assert_eq!(refs(text), vec![pair("@scope/pkg", "@scope/pkg", "")]);
    }

    #[test]
    fn decodes_surrogate_pairs_and_keeps_scanning() {
        let text = r#"{ "dependencies": { "\ud83d\udd2d": "catalog:", "react": "catalog:" } }"#;
        assert!(text.contains('\\'));
        assert_eq!(
            refs(text),
            vec![pair("🔭", "🔭", ""), pair("react", "react", "")]
        );
    }

    #[test]
    fn keeps_an_escaped_non_ascii_character_and_the_closing_quote() {
        let text = r#"{ "dependencies": { "\é": "catalog:", "react": "catalog:" } }"#;
        assert_eq!(
            refs(text),
            vec![pair("é", "é", ""), pair("react", "react", "")]
        );
    }

    #[test]
    fn key_typed_without_value_does_not_swallow_the_next_entry() {
        let text = "{ \"dependencies\": {\n \"react\": \n \"vue\": \"catalog:\" } }";
        assert_eq!(refs(text), vec![pair("vue", "vue", "")]);
    }

    #[test]
    fn normalizes_selector_keys() {
        assert_eq!(normalize_selector("foo>react@^17"), "react");
        assert_eq!(normalize_selector("**/@scope/pkg"), "@scope/pkg");
        assert_eq!(normalize_selector("@scope/pkg@>=1"), "@scope/pkg");
        assert_eq!(normalize_selector("pkg@|| >2"), "pkg");
        assert_eq!(normalize_selector("@scope/pkg@^1/dep"), "dep");
        assert_eq!(normalize_selector("left-pad/**"), "left-pad");
        assert_eq!(normalize_selector("foo@1>bar"), "bar");
        assert_eq!(normalize_selector("pkg@>2"), "pkg");
        assert_eq!(normalize_selector("react"), "react");
    }

    #[test]
    fn reference_at_matches_inclusive_token_bounds() {
        let text = r#"{ "dependencies": { "react": "catalog:" } }"#;
        let found = references(text);
        let start = found[0].range.start;
        let end = found[0].range.end;
        assert!(reference_at(&found, start, |r| r.range).is_some());
        assert!(reference_at(&found, end, |r| r.range).is_some());
        assert!(reference_at(&found, Position::new(0, start.character - 1), |r| r.range).is_none());
        assert!(reference_at(&found, Position::new(0, end.character + 1), |r| r.range).is_none());
    }

    #[test]
    fn value_slot_reports_typed_prefix_and_content_range() {
        let text = r#"{ "dependencies": { "react": "cata" } }"#;
        let start = text.find("cata").unwrap() as u32;
        let slot = value_slot_at(text, Position::new(0, start + 4)).unwrap();
        assert_eq!(slot.package, "react");
        assert_eq!(slot.typed, "cata");
        assert_eq!(
            slot.content_range,
            Range::new(Position::new(0, start), Position::new(0, start + 4))
        );
    }

    #[test]
    fn value_slot_in_unclosed_string_ends_at_line_end() {
        let text = "{ \"dependencies\": { \"react\": \"catalog:re\n";
        let start = text.find("catalog:re").unwrap() as u32;
        let slot = value_slot_at(text, Position::new(0, start + 10)).unwrap();
        assert_eq!(slot.typed, "catalog:re");
        assert_eq!(slot.content_range.end, Position::new(0, start + 10));
    }

    #[test]
    fn value_slot_ignores_keys_and_non_dependency_fields() {
        let text = r#"{ "name": "x", "dependencies": { "react": "catalog:" } }"#;
        assert!(value_slot_at(text, Position::new(0, 11)).is_none());
        let key = text.find("react").unwrap() as u32;
        assert!(value_slot_at(text, Position::new(0, key + 2)).is_none());
    }

    #[test]
    fn line_index_round_trips_positions() {
        let text = "ab\n🔭c\nd";
        let index = LineIndex::new(text);
        let offset = text.find('c').unwrap();
        let position = index.position(offset);
        assert_eq!(position, Position::new(1, 2));
        assert_eq!(index.offset(position), Some(offset));
        assert_eq!(index.offset(Position::new(1, 99)), Some(text.len() - 2));
        assert_eq!(index.offset(Position::new(9, 0)), None);
    }

    #[test]
    fn line_index_breaks_lines_at_crlf_lone_cr_and_lf() {
        let text = "ab\r\ncd\ref\ng";
        let index = LineIndex::new(text);
        for (needle, position) in [
            ('c', Position::new(1, 0)),
            ('e', Position::new(2, 0)),
            ('g', Position::new(3, 0)),
        ] {
            let offset = text.find(needle).unwrap();
            assert_eq!(index.position(offset), position, "{needle}");
            assert_eq!(index.offset(position), Some(offset), "{needle}");
        }
        assert_eq!(index.offset(Position::new(0, 99)), Some(2));
        assert_eq!(index.offset(Position::new(1, 99)), Some(6));
        assert_eq!(index.position(3), Position::new(0, 2));
        assert_eq!(index.offset_at_char(0, 99), Some(2));
        assert_eq!(index.offset_at_char(1, 99), Some(6));
        assert_eq!(index.offset_at_char(2, 1), Some(8));
    }

    #[test]
    fn a_repeated_key_keeps_only_its_last_value_like_json_parse() {
        assert!(
            refs(r#"{ "dependencies": { "react": "catalog:", "react": "^17.0.0" } }"#).is_empty()
        );
        assert!(
            refs(r#"{ "dependencies": { "react": "catalog:" }, "dependencies": { "vue": "^3.0.0" } }"#)
                .is_empty()
        );
        let text = r#"{ "dependencies": { "react": "^17.0.0", "vue": "catalog:", "react": "catalog:r" } }"#;
        assert_eq!(
            refs(text),
            vec![pair("vue", "vue", ""), pair("react", "react", "r")]
        );
        let found = references(text);
        let start = text.find("\"catalog:r\"").unwrap() as u32;
        assert_eq!(found[1].range.start, Position::new(0, start));
    }

    #[test]
    fn parse_exposes_nested_fields_for_bun_roots() {
        let text = r#"{ "workspaces": { "catalog": { "react": "^19" } } }"#;
        let root = parse(text, Duplicates::All);
        let catalog = root
            .field("workspaces")
            .and_then(|w| w.field("catalog"))
            .unwrap();
        match catalog.field("react") {
            Some(Node::Str(s)) => assert_eq!(s.value, "^19"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
