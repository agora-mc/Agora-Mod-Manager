//! The small JSON surface this module needs: one request object in and one
//! reply object out. It is hand-written because the module may not add a JSON
//! crate to the vendored libloot dependency set.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(values) => Some(values),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

pub fn parse(text: &str) -> Result<Value, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(format!(
            "unexpected text after the JSON value at byte {}",
            parser.pos
        ));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while let Some(byte) = self.bytes.get(self.pos) {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", byte as char, self.pos))
        }
    }

    fn literal(&mut self, text: &str) -> Result<(), String> {
        let end = self.pos + text.len();
        if self.bytes.get(self.pos..end) == Some(text.as_bytes()) {
            self.pos = end;
            Ok(())
        } else {
            Err(format!("expected '{text}' at byte {}", self.pos))
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'"') => self.parse_string().map(Value::String),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b't') => {
                self.literal("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Value::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(other) => Err(format!(
                "unexpected byte 0x{other:02x} at byte {}",
                self.pos
            )),
            None => Err("the JSON value is missing".to_string()),
        }
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            match byte {
                b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9' => self.pos += 1,
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "a JSON number is not valid UTF-8".to_string())?;
        let number = text
            .parse::<f64>()
            .map_err(|_| format!("'{text}' is not a JSON number"))?;
        if !number.is_finite() {
            return Err(format!("'{text}' is not a finite JSON number"));
        }
        Ok(Value::Number(number))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(byte) = self.peek() {
                if byte == b'"' || byte == b'\\' {
                    break;
                }
                if byte < 0x20 {
                    return Err(format!(
                        "a control character is not allowed in a JSON string at byte {}",
                        self.pos
                    ));
                }
                self.pos += 1;
            }
            if self.pos > start {
                let run = std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| "a JSON string is not valid UTF-8".to_string())?;
                out.push_str(run);
            }
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                None => return Err("the JSON string is not closed".to_string()),
                _ => unreachable!("the scan stops only on a quote, a backslash or the end"),
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), String> {
        let Some(byte) = self.peek() else {
            return Err("the JSON string ends after a backslash".to_string());
        };
        self.pos += 1;
        match byte {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.parse_hex4()?;
                if (0xD800..=0xDBFF).contains(&first) {
                    if self.peek() != Some(b'\\') {
                        return Err(
                            "a high surrogate is not followed by a low surrogate".to_string()
                        );
                    }
                    self.pos += 1;
                    if self.peek() != Some(b'u') {
                        return Err(
                            "a high surrogate is not followed by a low surrogate".to_string()
                        );
                    }
                    self.pos += 1;
                    let second = self.parse_hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&second) {
                        return Err(
                            "a high surrogate is not followed by a low surrogate".to_string()
                        );
                    }
                    let code =
                        0x10000 + ((first - 0xD800) as u32) * 0x400 + (second - 0xDC00) as u32;
                    let character = char::from_u32(code)
                        .ok_or_else(|| "a JSON escape is not a character".to_string())?;
                    out.push(character);
                } else if (0xDC00..=0xDFFF).contains(&first) {
                    return Err("a low surrogate appears without a high surrogate".to_string());
                } else {
                    let character = char::from_u32(first as u32)
                        .ok_or_else(|| "a JSON escape is not a character".to_string())?;
                    out.push(character);
                }
            }
            other => {
                return Err(format!(
                    "unknown JSON escape \\{} at byte {}",
                    other as char,
                    self.pos - 1
                ))
            }
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        let end = self.pos + 4;
        let digits = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| "a JSON \\u escape needs four hex digits".to_string())?;
        let text = std::str::from_utf8(digits)
            .map_err(|_| "a JSON \\u escape needs four hex digits".to_string())?;
        let value = u16::from_str_radix(text, 16)
            .map_err(|_| format!("'{text}' is not four hex digits"))?;
        self.pos = end;
        Ok(value)
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        self.skip_ws();
        let mut values = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(values));
        }
        loop {
            self.skip_ws();
            values.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(values));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        self.skip_ws();
        let mut entries = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(entries));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
            }
        }
    }
}

pub fn write_string(value: &str, out: &mut String) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            character if (character as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => out.push(character),
        }
    }
    out.push('"');
}
