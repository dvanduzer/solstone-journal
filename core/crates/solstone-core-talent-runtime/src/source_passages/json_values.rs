// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Readable normalization of fenced JSON inside rendered source text.
//!
//! A fenced block whose body is one strict JSON value is replaced by one line
//! per scalar (or empty container), headed by its path and type, so a passage
//! can quote a value without its string escaping. Everything else, including a
//! fence that does not parse, has a duplicate key, or never closes, is kept
//! exactly as it was. Number lexemes are kept as written.

const MAX_DEPTH: usize = 512;

/// The normalized text of one rendered source region.
pub(crate) fn normalize(text: &str) -> String {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < lines.len() {
        let Some(marker) = opening_marker(lines[index]) else {
            output.push_str(lines[index]);
            index += 1;
            continue;
        };
        let Some(close) = (index + 1..lines.len()).find(|line| closes(lines[*line], marker)) else {
            // An unterminated fence keeps the rest of the region as written.
            for line in &lines[index..] {
                output.push_str(line);
            }
            break;
        };
        let body = lines[index + 1..close].concat();
        match parse(&body) {
            Some(value) => {
                output.push_str("\nJSON values begin\n");
                render(&value, &mut Vec::new(), &mut output);
                output.push_str("JSON values end\n");
            }
            None => {
                for line in &lines[index..=close] {
                    output.push_str(line);
                }
            }
        }
        index = close + 1;
    }
    output
}

/// A fence opens with at most three spaces or tabs, then three or more
/// backticks or tildes; the run's character and length are what close it.
fn opening_marker(line: &str) -> Option<(char, usize)> {
    let rest = strip_indent(line)?;
    let character = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = rest.chars().take_while(|c| *c == character).count();
    (run >= 3).then_some((character, run))
}

fn closes(line: &str, (character, run): (char, usize)) -> bool {
    let Some(rest) = strip_indent(line) else {
        return false;
    };
    let length = rest.chars().take_while(|c| *c == character).count();
    let tail = rest[length * character.len_utf8()..]
        .strip_suffix('\n')
        .map(|tail| tail.strip_suffix('\r').unwrap_or(tail))
        .unwrap_or(&rest[length * character.len_utf8()..]);
    length >= run && tail.chars().all(|c| c == ' ' || c == '\t')
}

fn strip_indent(line: &str) -> Option<&str> {
    let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    (indent <= 3).then(|| &line[indent..])
}

enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

#[derive(Clone)]
enum Key {
    Index(usize),
    Name(String),
}

fn render(value: &Json, path: &mut Vec<Key>, output: &mut String) {
    let (kind, text) = match value {
        Json::Object(fields) if !fields.is_empty() => {
            for (name, child) in fields {
                path.push(Key::Name(name.clone()));
                render(child, path, output);
                path.pop();
            }
            return;
        }
        Json::Array(items) if !items.is_empty() => {
            for (index, child) in items.iter().enumerate() {
                path.push(Key::Index(index));
                render(child, path, output);
                path.pop();
            }
            return;
        }
        Json::Object(_) => ("object", "{}"),
        Json::Array(_) => ("array", "[]"),
        Json::Null => ("null", "null"),
        Json::Bool(true) => ("boolean", "true"),
        Json::Bool(false) => ("boolean", "false"),
        Json::Number(lexeme) => ("number", lexeme.as_str()),
        Json::String(text) => ("string", text.as_str()),
    };
    output.push('$');
    for key in path.iter() {
        output.push('[');
        match key {
            Key::Index(index) => output.push_str(&index.to_string()),
            Key::Name(name) => output.push_str(&ascii_json_string(name)),
        }
        output.push(']');
    }
    output.push_str(" [");
    output.push_str(kind);
    output.push_str("]:\n");
    output.push_str(text);
    output.push('\n');
}

/// A JSON string literal with every non-ASCII character escaped.
pub(crate) fn ascii_json_string(text: &str) -> String {
    let mut output = String::with_capacity(text.len() + 2);
    output.push('"');
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            ' '..='~' => output.push(character),
            _ => {
                let mut units = [0_u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    output.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    output.push('"');
    output
}

fn parse(text: &str) -> Option<Json> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        position: 0,
    };
    parser.whitespace();
    let value = parser.value(0)?;
    parser.whitespace();
    (parser.position == parser.bytes.len()).then_some(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn literal(&mut self, word: &str) -> bool {
        if self.text[self.position..].starts_with(word) {
            self.position += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Json::String),
            b't' => self.literal("true").then_some(Json::Bool(true)),
            b'f' => self.literal("false").then_some(Json::Bool(false)),
            b'n' => self.literal("null").then_some(Json::Null),
            _ => self.number().map(Json::Number),
        }
    }

    fn object(&mut self, depth: usize) -> Option<Json> {
        self.position += 1;
        let mut fields: Vec<(String, Json)> = Vec::new();
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Some(Json::Object(fields));
        }
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return None;
            }
            let name = self.string()?;
            // A repeated key has no single meaning to show.
            if fields.iter().any(|(existing, _)| *existing == name) {
                return None;
            }
            self.whitespace();
            if self.peek() != Some(b':') {
                return None;
            }
            self.position += 1;
            self.whitespace();
            let value = self.value(depth + 1)?;
            fields.push((name, value));
            self.whitespace();
            match self.peek()? {
                b',' => self.position += 1,
                b'}' => {
                    self.position += 1;
                    return Some(Json::Object(fields));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Json> {
        self.position += 1;
        let mut items = Vec::new();
        self.whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Some(Json::Array(items));
        }
        loop {
            self.whitespace();
            items.push(self.value(depth + 1)?);
            self.whitespace();
            match self.peek()? {
                b',' => self.position += 1,
                b']' => {
                    self.position += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        self.position += 1;
        let mut output = String::new();
        loop {
            let rest = &self.text[self.position..];
            let character = rest.chars().next()?;
            self.position += character.len_utf8();
            match character {
                '"' => return Some(output),
                '\\' => {
                    let escape = self.peek()?;
                    self.position += 1;
                    match escape {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{8}'),
                        b'f' => output.push('\u{c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => output.push(self.unicode_escape()?),
                        _ => return None,
                    }
                }
                control if (control as u32) < 0x20 => return None,
                other => output.push(other),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.text.get(self.position..self.position + 4)?;
        let value = u32::from_str_radix(digits, 16).ok()?;
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        self.position += 4;
        Some(value)
    }

    /// A lone surrogate has no text to show, so the block stays as written.
    fn unicode_escape(&mut self) -> Option<char> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first) {
            if !self.text[self.position..].starts_with("\\u") {
                return None;
            }
            self.position += 2;
            let second = self.hex4()?;
            if !(0xDC00..0xE000).contains(&second) {
                return None;
            }
            return char::from_u32(0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00));
        }
        char::from_u32(first)
    }

    fn number(&mut self) -> Option<String> {
        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        match self.peek()? {
            b'0' => self.position += 1,
            b'1'..=b'9' => self.digits(),
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            self.digits();
        }
        Some(self.text[start..self.position].to_owned())
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_json_value_becomes_one_readable_line_per_value() {
        let text = "Before\n```json\n{\"title\":\"Fix \\\"retry\\\" path\",\"n\":1.50,\"tags\":[],\"é\":{\"ok\":true}}\n```\nAfter\n";
        assert_eq!(
            normalize(text),
            concat!(
                "Before\n",
                "\nJSON values begin\n",
                "$[\"title\"] [string]:\nFix \"retry\" path\n",
                "$[\"n\"] [number]:\n1.50\n",
                "$[\"tags\"] [array]:\n[]\n",
                "$[\"\\u00e9\"][\"ok\"] [boolean]:\ntrue\n",
                "JSON values end\n",
                "After\n",
            )
        );
    }

    #[test]
    fn a_fence_that_is_not_one_strict_json_value_stays_exactly_as_written() {
        for text in [
            "```json\n{\"a\":1,\"a\":2}\n```\n",
            "```\n{\"a\":NaN}\n```\n",
            "```\nnot json\n```\n",
            "~~~~\n{\"a\":\"\\ud800\"}\n~~~~\n",
            "```json\n{\"a\":1}\n",
            "```json\n[1]\n``\n",
        ] {
            assert_eq!(normalize(text), text, "{text:?}");
        }
    }

    #[test]
    fn text_outside_a_fence_is_unchanged_byte_for_byte() {
        let text = "Literal \\n and \\u0041 stay literal.\r\nCafé 🪨 \"quoted\"\n    ```indented code is not a fence\n";
        assert_eq!(normalize(text), text);
    }
}
