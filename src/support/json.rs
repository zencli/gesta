//! Minimal JSON writer.
//!
//! No dependency on serde: the shapes this program emits are few and fixed, and
//! writing them directly keeps the binary small and the output predictable.
//! Every string goes through `escape`, so a message or path containing quotes,
//! backslashes or control characters cannot break the document.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    pub fn int(n: impl Into<i64>) -> Json {
        Json::Int(n.into())
    }

    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v);
        }
        Json::Obj(m)
    }

    pub fn arr(items: Vec<Json>) -> Json {
        Json::Arr(items)
    }

    /// Render compactly, one value, no trailing newline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// Render with two-space indentation, for a human reading a terminal.
    pub fn render_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Float(f) => {
                if f.is_finite() {
                    out.push_str(&format!("{}", f));
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => escape_into(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.write(out);
                }
                out.push(']');
            }
            Json::Obj(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape_into(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, depth: usize) {
        let pad = "  ".repeat(depth + 1);
        let close_pad = "  ".repeat(depth);
        match self {
            Json::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, it) in items.iter().enumerate() {
                    out.push_str(&pad);
                    it.write_pretty(out, depth + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&close_pad);
                out.push(']');
            }
            Json::Obj(map) if !map.is_empty() => {
                out.push_str("{\n");
                let n = map.len();
                for (i, (k, v)) in map.iter().enumerate() {
                    out.push_str(&pad);
                    escape_into(k, out);
                    out.push_str(": ");
                    v.write_pretty(out, depth + 1);
                    if i + 1 < n {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&close_pad);
                out.push('}');
            }
            other => other.write(out),
        }
    }
}

fn escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JSON value parser, enough for JSON-RPC input.
///
/// Handles objects, arrays, strings with escapes, numbers, booleans and null.
/// Returns an error message rather than panicking on malformed input, because
/// this parses data arriving from another process.
pub fn parse(input: &str) -> Result<Json, String> {
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let v = parse_value(&bytes, &mut i)?;
    skip_ws(&bytes, &mut i);
    if i != bytes.len() {
        return Err(format!("trailing input at position {}", i));
    }
    Ok(v)
}

fn skip_ws(b: &[char], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], ' ' | '\t' | '\n' | '\r') {
        *i += 1;
    }
}

fn parse_value(b: &[char], i: &mut usize) -> Result<Json, String> {
    skip_ws(b, i);
    if *i >= b.len() {
        return Err("unexpected end of input".into());
    }
    match b[*i] {
        '{' => parse_object(b, i),
        '[' => parse_array(b, i),
        '"' => Ok(Json::Str(parse_string(b, i)?)),
        't' => expect(b, i, "true").map(|_| Json::Bool(true)),
        'f' => expect(b, i, "false").map(|_| Json::Bool(false)),
        'n' => expect(b, i, "null").map(|_| Json::Null),
        c if c == '-' || c.is_ascii_digit() => parse_number(b, i),
        c => Err(format!("unexpected character {:?}", c)),
    }
}

fn expect(b: &[char], i: &mut usize, word: &str) -> Result<(), String> {
    for want in word.chars() {
        if *i >= b.len() || b[*i] != want {
            return Err(format!("expected {:?}", word));
        }
        *i += 1;
    }
    Ok(())
}

fn parse_object(b: &[char], i: &mut usize) -> Result<Json, String> {
    *i += 1; // {
    let mut map = BTreeMap::new();
    skip_ws(b, i);
    if *i < b.len() && b[*i] == '}' {
        *i += 1;
        return Ok(Json::Obj(map));
    }
    loop {
        skip_ws(b, i);
        let key = parse_string(b, i)?;
        skip_ws(b, i);
        if *i >= b.len() || b[*i] != ':' {
            return Err("expected ':' after object key".into());
        }
        *i += 1;
        let val = parse_value(b, i)?;
        map.insert(key, val);
        skip_ws(b, i);
        if *i >= b.len() {
            return Err("unterminated object".into());
        }
        match b[*i] {
            ',' => {
                *i += 1;
            }
            '}' => {
                *i += 1;
                return Ok(Json::Obj(map));
            }
            c => return Err(format!("unexpected {:?} in object", c)),
        }
    }
}

fn parse_array(b: &[char], i: &mut usize) -> Result<Json, String> {
    *i += 1; // [
    let mut items = Vec::new();
    skip_ws(b, i);
    if *i < b.len() && b[*i] == ']' {
        *i += 1;
        return Ok(Json::Arr(items));
    }
    loop {
        items.push(parse_value(b, i)?);
        skip_ws(b, i);
        if *i >= b.len() {
            return Err("unterminated array".into());
        }
        match b[*i] {
            ',' => {
                *i += 1;
            }
            ']' => {
                *i += 1;
                return Ok(Json::Arr(items));
            }
            c => return Err(format!("unexpected {:?} in array", c)),
        }
    }
}

fn parse_string(b: &[char], i: &mut usize) -> Result<String, String> {
    if *i >= b.len() || b[*i] != '"' {
        return Err("expected a string".into());
    }
    *i += 1;
    let mut out = String::new();
    while *i < b.len() {
        match b[*i] {
            '"' => {
                *i += 1;
                return Ok(out);
            }
            '\\' => {
                *i += 1;
                if *i >= b.len() {
                    return Err("trailing backslash".into());
                }
                match b[*i] {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let hex: String = b.get(*i + 1..*i + 5).unwrap_or(&[]).iter().collect();
                        let code = u32::from_str_radix(&hex, 16)
                            .map_err(|_| format!("bad unicode escape {:?}", hex))?;
                        *i += 4;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    c => return Err(format!("bad escape \\{}", c)),
                }
                *i += 1;
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    Err("unterminated string".into())
}

fn parse_number(b: &[char], i: &mut usize) -> Result<Json, String> {
    let start = *i;
    if *i < b.len() && b[*i] == '-' {
        *i += 1;
    }
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
    }
    let mut is_float = false;
    if *i < b.len() && b[*i] == '.' {
        is_float = true;
        *i += 1;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    if *i < b.len() && (b[*i] == 'e' || b[*i] == 'E') {
        is_float = true;
        *i += 1;
        if *i < b.len() && (b[*i] == '+' || b[*i] == '-') {
            *i += 1;
        }
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    let text: String = b[start..*i].iter().collect();
    if is_float {
        text.parse::<f64>()
            .map(Json::Float)
            .map_err(|_| format!("bad number {:?}", text))
    } else {
        match text.parse::<i64>() {
            Ok(n) => Ok(Json::Int(n)),
            Err(_) => text
                .parse::<f64>()
                .map(Json::Float)
                .map_err(|_| format!("bad number {:?}", text)),
        }
    }
}

/// Look up a key in an object.
pub fn field<'a>(v: &'a Json, key: &str) -> Option<&'a Json> {
    match v {
        Json::Obj(m) => m.get(key),
        _ => None,
    }
}

pub fn as_str(v: &Json) -> Option<&str> {
    match v {
        Json::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Read a boolean, so a tool argument does not have to be spelled as a string.
pub fn as_bool(v: &Json) -> Option<bool> {
    match v {
        Json::Bool(b) => Some(*b),
        _ => None,
    }
}

pub fn as_i64(v: &Json) -> Option<i64> {
    match v {
        Json::Int(n) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_scalars() {
        assert_eq!(Json::Null.render(), "null");
        assert_eq!(Json::Bool(true).render(), "true");
        assert_eq!(Json::int(42).render(), "42");
        assert_eq!(Json::int(-7).render(), "-7");
        assert_eq!(Json::str("hi").render(), "\"hi\"");
    }

    #[test]
    fn escapes_strings_that_would_break_the_document() {
        let j = Json::str("a\"b\\c\nd\te");
        assert_eq!(j.render(), "\"a\\\"b\\\\c\\nd\\te\"");
    }

    #[test]
    fn escapes_control_characters() {
        let j = Json::str("bell\u{7}here");
        assert_eq!(j.render(), "\"bell\\u0007here\"");
    }

    #[test]
    fn renders_objects_with_sorted_keys() {
        let j = Json::obj(vec![("b", Json::int(2)), ("a", Json::int(1))]);
        assert_eq!(j.render(), "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn renders_nested_arrays() {
        let j = Json::arr(vec![Json::int(1), Json::arr(vec![Json::int(2)])]);
        assert_eq!(j.render(), "[1,[2]]");
    }

    #[test]
    fn parses_an_rpc_request() {
        let text = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let v = parse(text).unwrap();
        assert_eq!(as_str(field(&v, "method").unwrap()), Some("tools/list"));
        assert_eq!(as_i64(field(&v, "id").unwrap()), Some(1));
        assert!(field(&v, "params").is_some());
    }

    #[test]
    fn parses_nested_arguments_and_escapes() {
        let text = r#"{"name":"x","arguments":{"path":"a\"b.txt","n":3,"ok":true}}"#;
        let v = parse(text).unwrap();
        let args = field(&v, "arguments").unwrap();
        assert_eq!(as_str(field(args, "path").unwrap()), Some("a\"b.txt"));
        assert_eq!(as_i64(field(args, "n").unwrap()), Some(3));
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        for bad in ["{", "[1,", "{\"a\"}", "nul", "\"unterminated", "{} extra"] {
            assert!(parse(bad).is_err(), "should reject {:?}", bad);
        }
    }

    #[test]
    fn roundtrip_through_parse_and_render() {
        let original = Json::obj(vec![
            ("msg", Json::str("revert \"x\"\nnow")),
            ("n", Json::int(5)),
            ("list", Json::arr(vec![Json::int(1), Json::int(2)])),
        ]);
        let text = original.render();
        assert_eq!(parse(&text).unwrap(), original);
    }

    #[test]
    fn pretty_output_is_indented_and_still_parses() {
        let j = Json::obj(vec![("a", Json::arr(vec![Json::int(1), Json::int(2)]))]);
        let pretty = j.render_pretty();
        assert!(pretty.contains("\n  "));
        assert_eq!(parse(&pretty).unwrap(), j);
    }
}
