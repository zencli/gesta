//! Minimal JSON writer.
//!
//! No dependency on serde: the shapes this program emits are few and fixed, and
//! writing them directly keeps the binary small and the output predictable.
//! Every string goes through `escape`, so a message or path containing quotes,
//! backslashes or control characters cannot break the document.

use std::collections::BTreeMap;

/// The version stamped on every object this program puts on the wire.
///
/// It lives in the JSON tier, not the CLI tier, because the error document is built in
/// `support::error` and the MCP server stamps its own replies in `transport::mcp`. Both are below
/// the CLI, so a constant that only the CLI could reach forced those two to depend upward on it.
pub const WIRE_SCHEMA: &str = "gesta@1";

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
/// How deep nesting may go.
///
/// The parser recurses once per level, so without a cap a body of a few hundred kilobytes of
/// `[[[[…` overflows the stack and aborts the process (#80). Reachable without a write token: the
/// MCP path parses one line of stdin per message. 512 is far past any shape this program writes -
/// a changeset body is an object of arrays of strings - and shallow enough that the frame count
/// stays inside a default 8 MiB stack.
pub const MAX_DEPTH: usize = 512;

/// How large an input this parser will look at.
///
/// The parser collects the input into a `Vec<char>` before reading it, which is four bytes per
/// character: a 10 MiB MCP line became 40 MiB of heap, and the MCP path parses one stdin line per
/// message with nothing above it counting bytes (#299). The depth cap bounds the stack and this
/// bounds the heap, because they are two different ways to stop the process and only one of them was
/// capped.
///
/// 64 MiB is far past anything this program sends - the largest message it writes is a bundle
/// summary - and small enough that the four-byte expansion stays inside a modest container's memory
/// limit. The refusal names the limit so a caller can tell "too large" from "malformed".
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

pub fn parse(input: &str) -> Result<Json, String> {
    // Checked before the collect, which is the allocation the limit exists to prevent.
    if input.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "input is {} bytes, over the {} byte limit this parser accepts",
            input.len(),
            MAX_INPUT_BYTES
        ));
    }
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let v = parse_value(&bytes, &mut i, 0)?;
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

fn parse_value(b: &[char], i: &mut usize, depth: usize) -> Result<Json, String> {
    skip_ws(b, i);
    if *i >= b.len() {
        return Err("unexpected end of input".into());
    }
    match b[*i] {
        '{' => parse_object(b, i, depth),
        '[' => parse_array(b, i, depth),
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

fn parse_object(b: &[char], i: &mut usize, depth: usize) -> Result<Json, String> {
    if depth >= MAX_DEPTH {
        return Err(format!(
            "nested deeper than {} levels; refusing rather than recursing until the stack runs out",
            MAX_DEPTH
        ));
    }
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
        let val = parse_value(b, i, depth + 1)?;
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

fn parse_array(b: &[char], i: &mut usize, depth: usize) -> Result<Json, String> {
    if depth >= MAX_DEPTH {
        return Err(format!(
            "nested deeper than {} levels; refusing rather than recursing until the stack runs out",
            MAX_DEPTH
        ));
    }
    *i += 1; // [
    let mut items = Vec::new();
    skip_ws(b, i);
    if *i < b.len() && b[*i] == ']' {
        *i += 1;
        return Ok(Json::Arr(items));
    }
    loop {
        items.push(parse_value(b, i, depth + 1)?);
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
    #[test]
    fn deeply_nested_input_is_refused_instead_of_aborting_the_process() {
        // #80: `parse_value` -> `parse_array` recursed once per level with no cap, so a few hundred
        // kilobytes of `[[[[...` overflowed the stack and aborted the process. Reachable without a
        // write token through the MCP path, which parses one line of stdin per message.
        //
        // 64 MiB of stack so a regression reports a failure here instead of the test binary dying
        // with no message - a stack overflow cannot be caught.
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let deep = format!("{}{}", "[".repeat(200_000), "]".repeat(200_000));
                match parse(&deep) {
                    Ok(_) => Err("200000 levels parsed; the cap is not in the way".to_string()),
                    Err(e) => Ok(e),
                }
            })
            .unwrap();
        match handle.join().expect("parsing must not kill the thread") {
            Ok(msg) => assert!(
                msg.contains("nested deeper than"),
                "the refusal must name the depth, got: {msg}"
            ),
            Err(bad) => panic!("{bad}"),
        }
    }

    #[test]
    fn nesting_up_to_the_cap_still_parses() {
        // The cap must not be so tight that a real document trips it.
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH - 1), "]".repeat(MAX_DEPTH - 1));
        assert!(parse(&ok).is_ok(), "{} levels must parse", MAX_DEPTH - 1);
        let too_deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(
            parse(&too_deep).is_err(),
            "{} must be refused",
            MAX_DEPTH + 1
        );
    }
    #[test]
    fn an_input_over_the_limit_is_refused_before_it_is_copied() {
        // #299: the parser collected the input into a `Vec<char>` before reading a byte of it, so a
        // 10 MiB line became 40 MiB of heap and nothing above this layer counted bytes. The depth cap
        // from #80 bounds the stack; this bounds the heap.
        //
        // The input is built at the limit rather than over it to keep the test cheap: the assertion is
        // that the check runs BEFORE the collect, so a string one byte over is refused without the
        // four-byte expansion happening at all.
        let at_limit = " ".repeat(MAX_INPUT_BYTES);
        // It parses as nothing (whitespace only is not a value), which proves the size check passed.
        assert!(parse(&at_limit).is_err());
        assert!(
            !parse(&at_limit).unwrap_err().contains("over the"),
            "an input at the limit must reach the parser, not the size check"
        );

        let over = " ".repeat(MAX_INPUT_BYTES + 1);
        let e = parse(&over).unwrap_err();
        assert!(
            e.contains("over the") && e.contains(&MAX_INPUT_BYTES.to_string()),
            "the refusal must name the limit: {e}"
        );
    }
}
