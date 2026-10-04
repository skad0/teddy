//! Dev-only JSON-lines <-> binary protocol bridge (spec §14.1). The core
//! never speaks JSON: register this executable as the plugin and point
//! `TEDDY_JSON_PLUGIN` at an absolute JSON-lines plugin executable (run with
//! no arguments). One JSON object per line, both directions:
//!
//! `{"type":4,"flags":1,"request_id":0,"resource_id":2,"resource_revision":3, ...}`
//!
//! plus exactly one payload field: `"text"` (UTF-8), `"hex"`, or — plugin to
//! core only — `"widget":{"kind":2,"cols":0,"items":[...],"tree":[[0,1],...]}`.
//! Core frames arrive with `"text"` when the payload is printable UTF-8,
//! otherwise `"hex"`. A malformed line is reported on stderr and ends the
//! bridge, so the core's jobs pane shows why.

#[path = "../plugin.rs"]
#[allow(dead_code)]
mod plugin;

use plugin::{encode, encode_widget, Frame, FrameReader, Widget, MAX_PAYLOAD};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

const MAX_LINE: usize = 4 * MAX_PAYLOAD as usize;

fn main() {
    let Some(path) = std::env::var_os("TEDDY_JSON_PLUGIN") else {
        die("TEDDY_JSON_PLUGIN is not set");
    };
    let mut child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot start JSON plugin: {e}")));
    let mut to_child = child.stdin.take().unwrap();
    let from_child = BufReader::new(child.stdout.take().unwrap());

    // ponytail: two plain threads — this is a dev adapter outside the core,
    // where §21's no-thread rule applies.
    std::thread::spawn(move || {
        let mut out = io::stdout().lock();
        // ponytail: split() buffers a whole line before the MAX_LINE check; fine
        // for a dev adapter fed by a local plugin.
        for line in from_child.split(b'\n') {
            let Ok(line) = line else { break };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let frame = (line.len() <= MAX_LINE)
                .then(|| std::str::from_utf8(&line).ok())
                .flatten()
                .ok_or("line is not bounded UTF-8")
                .and_then(json_to_frame)
                .unwrap_or_else(|e| die(&format!("bad JSON line from plugin: {e}")));
            let mut bytes = Vec::new();
            encode(&frame, &mut bytes);
            if out.write_all(&bytes).and_then(|_| out.flush()).is_err() {
                break;
            }
        }
        std::process::exit(0);
    });

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 8192];
    let mut stdin = io::stdin().lock();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => reader.push(&buf[..n]),
        }
        loop {
            match reader.next() {
                Ok(Some(frame)) => {
                    let line = frame_to_json(&frame) + "\n";
                    if to_child.write_all(line.as_bytes()).is_err() {
                        die("JSON plugin closed its stdin");
                    }
                }
                Ok(None) => break,
                Err(e) => die(&format!("bad frame from core: {e:?}")),
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn die(msg: &str) -> ! {
    eprintln!("teddy-json-bridge: {msg}");
    std::process::exit(1)
}

fn frame_to_json(f: &Frame) -> String {
    let printable = std::str::from_utf8(&f.payload)
        .ok()
        .filter(|s| !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'));
    let payload = match printable {
        Some(s) => format!("\"text\":{}", quote(s)),
        None => format!(
            "\"hex\":\"{}\"",
            f.payload
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ),
    };
    format!(
        "{{\"type\":{},\"flags\":{},\"request_id\":{},\"resource_id\":{},\"resource_revision\":{},{payload}}}",
        f.msg_type, f.flags, f.request_id, f.resource_id, f.resource_revision
    )
}

fn json_to_frame(line: &str) -> Result<Frame, &'static str> {
    let mut p = Parser {
        s: line.as_bytes(),
        at: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.at != p.s.len() {
        return Err("trailing data");
    }
    let Json::Obj(fields) = v else {
        return Err("line is not an object");
    };
    let get = |k: &str| fields.iter().find(|(name, _)| name == k).map(|(_, v)| v);
    let num = |k: &str| match get(k) {
        None => Ok(0),
        Some(Json::Num(n)) => n.parse::<u64>().map_err(|_| "number out of range"),
        Some(_) => Err("number expected"),
    };
    let payload_fields = ["text", "hex", "widget"]
        .iter()
        .filter(|k| get(k).is_some())
        .count();
    if payload_fields > 1 {
        return Err("use one of text/hex/widget");
    }
    let payload = match (get("text"), get("hex"), get("widget")) {
        (Some(Json::Str(s)), _, _) => s.clone().into_bytes(),
        (_, Some(Json::Str(h)), _) => {
            if h.len() % 2 != 0 {
                return Err("odd hex length");
            }
            (0..h.len())
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(h.get(i..i + 2).ok_or("bad hex")?, 16).map_err(|_| "bad hex")
                })
                .collect::<Result<_, _>>()?
        }
        (_, _, Some(w)) => encode_widget(&json_to_widget(w)?),
        (None, None, None) => Vec::new(),
        _ => return Err("text/hex must be strings"),
    };
    if payload.len() > MAX_PAYLOAD as usize {
        return Err("payload too large");
    }
    Ok(Frame {
        msg_type: u16::try_from(num("type")?).map_err(|_| "type out of range")?,
        flags: u16::try_from(num("flags")?).map_err(|_| "flags out of range")?,
        request_id: u32::try_from(num("request_id")?).map_err(|_| "request_id out of range")?,
        resource_id: num("resource_id")?,
        resource_revision: num("resource_revision")?,
        payload,
    })
}

fn json_to_widget(v: &Json) -> Result<Widget, &'static str> {
    let Json::Obj(fields) = v else {
        return Err("widget must be an object");
    };
    let get = |k: &str| fields.iter().find(|(name, _)| name == k).map(|(_, v)| v);
    let byte = |v: Option<&Json>| match v {
        None => Ok(0),
        Some(Json::Num(n)) => n.parse::<u8>().map_err(|_| "widget byte out of range"),
        Some(_) => Err("number expected"),
    };
    let items = match get("items") {
        None => Vec::new(),
        Some(Json::Arr(a)) => a
            .iter()
            .map(|v| match v {
                Json::Str(s) => Ok(s.clone()),
                _ => Err("items must be strings"),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("items must be an array"),
    };
    let tree = match get("tree") {
        None => Vec::new(),
        Some(Json::Arr(a)) => a
            .iter()
            .map(|v| match v {
                Json::Arr(pair) if pair.len() == 2 => {
                    Ok((byte(Some(&pair[0]))?, byte(Some(&pair[1]))?))
                }
                _ => Err("tree rows are [depth, flags]"),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("tree must be an array"),
    };
    let kind = byte(get("kind"))?;
    // encode_widget pads/truncates tree metadata; refuse instead of mutating
    if tree.len()
        != if kind == plugin::W_TREE {
            items.len()
        } else {
            0
        }
    {
        return Err("tree needs one [depth, flags] per item, and only for kind 2");
    }
    let w = Widget {
        kind,
        cols: byte(get("cols"))?,
        items,
        tree,
        ..Widget::default()
    };
    // validate with the core's own parser so the bridge never forwards a
    // widget the core would reject
    plugin::parse_widget(&encode_widget(&w)).ok_or("invalid widget shape")?;
    Ok(w)
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    /// Kept as text so u64 ids never round through f64.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.at).is_some_and(|b| b" \t\r\n".contains(b)) {
            self.at += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        let hit = self.s[self.at..].starts_with(lit.as_bytes());
        if hit {
            self.at += lit.len();
        }
        hit
    }

    fn value(&mut self) -> Result<Json, &'static str> {
        self.value_at(0)
    }

    fn value_at(&mut self, depth: usize) -> Result<Json, &'static str> {
        if depth > 16 {
            return Err("nesting too deep");
        }
        self.ws();
        match self.s.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Ok(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    let Json::Str(key) = self.string()? else {
                        unreachable!()
                    };
                    self.ws();
                    if !self.eat(":") {
                        return Err("expected ':'");
                    }
                    fields.push((key, self.value_at(depth + 1)?));
                    self.ws();
                    if self.eat("}") {
                        return Ok(Json::Obj(fields));
                    }
                    if !self.eat(",") {
                        return Err("expected ',' or '}'");
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Ok(Json::Arr(items));
                }
                loop {
                    items.push(self.value_at(depth + 1)?);
                    self.ws();
                    if self.eat("]") {
                        return Ok(Json::Arr(items));
                    }
                    if !self.eat(",") {
                        return Err("expected ',' or ']'");
                    }
                }
            }
            Some(b'"') => self.string(),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.at;
                while self
                    .s
                    .get(self.at)
                    .is_some_and(|b| b"+-.eE0123456789".contains(b))
                {
                    self.at += 1;
                }
                Ok(Json::Num(
                    String::from_utf8_lossy(&self.s[start..self.at]).into_owned(),
                ))
            }
            _ if self.eat("null") => Ok(Json::Null),
            _ if self.eat("true") => Ok(Json::Bool(true)),
            _ if self.eat("false") => Ok(Json::Bool(false)),
            _ => Err("unexpected token"),
        }
    }

    fn hex4(&mut self) -> Result<u32, &'static str> {
        let digits = self.s.get(self.at..self.at + 4).ok_or("short \\u escape")?;
        let digits = std::str::from_utf8(digits).map_err(|_| "bad \\u escape")?;
        self.at += 4;
        u32::from_str_radix(digits, 16).map_err(|_| "bad \\u escape")
    }

    fn string(&mut self) -> Result<Json, &'static str> {
        if !self.eat("\"") {
            return Err("expected string");
        }
        let mut out = Vec::new();
        loop {
            let b = *self.s.get(self.at).ok_or("unterminated string")?;
            self.at += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let e = *self.s.get(self.at).ok_or("unterminated escape")?;
                    self.at += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&hi) {
                                if !self.eat("\\u") {
                                    return Err("lone surrogate");
                                }
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err("bad surrogate pair");
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            char::from_u32(code).ok_or("bad code point")?
                        }
                        _ => return Err("bad escape"),
                    };
                    let mut tmp = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
                0x00..=0x1f => return Err("raw control in string"),
                _ => out.push(b),
            }
        }
        String::from_utf8(out)
            .map(Json::Str)
            .map_err(|_| "invalid UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin::{STATUS, WIDGET, W_TREE};

    #[test]
    fn strings_round_trip_through_quote_and_parse() {
        for s in ["plain", "q\"uote\\back", "nl\nt\tc\u{1}", "ü → 😀"] {
            let quoted = quote(s);
            let mut p = Parser {
                s: quoted.as_bytes(),
                at: 0,
            };
            assert_eq!(p.value(), Ok(Json::Str(s.to_owned())));
        }
        let mut p = Parser {
            s: "\"\\u00e9\\ud83d\\ude00\\/\"".as_bytes(),
            at: 0,
        };
        assert_eq!(p.value(), Ok(Json::Str("é😀/".to_owned())));
        for bad in [&br#""\ud83d""#[..], br#""\x""#, b"\"a\nb\"", br#""\u12""#] {
            assert!(Parser { s: bad, at: 0 }.value().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn frames_round_trip_through_json() {
        let text = Frame {
            msg_type: STATUS,
            flags: 3,
            request_id: 7,
            resource_id: u64::MAX,
            resource_revision: 9,
            payload: "hi \"there\"\n".as_bytes().to_vec(),
        };
        let binary = Frame {
            payload: vec![1, 0, 0, 0, 0xff],
            ..text.clone()
        };
        for f in [text, binary] {
            let json = frame_to_json(&f);
            assert_eq!(json_to_frame(&json).as_ref(), Ok(&f), "{json}");
        }
        assert!(frame_to_json(&Frame {
            payload: vec![1, 0, 0, 0],
            ..Default::default()
        })
        .contains("\"hex\":\"01000000\""));
    }

    #[test]
    fn widget_shorthand_matches_the_binary_encoder() {
        let f = json_to_frame(
            r#"{"type":4,"flags":1,"resource_id":2,"resource_revision":1,
                "widget":{"kind":2,"items":["src","main.rs"],"tree":[[0,3],[1,0]]}}"#,
        )
        .unwrap();
        assert_eq!(f.msg_type, WIDGET);
        let want = Widget {
            kind: W_TREE,
            items: vec!["src".into(), "main.rs".into()],
            tree: vec![(0, 3), (1, 0)],
            ..Widget::default()
        };
        assert_eq!(f.payload, encode_widget(&want));
        // shapes the core would reject never leave the bridge
        assert!(json_to_frame(r#"{"type":4,"widget":{"kind":3,"cols":2,"items":["a"]}}"#).is_err());
        assert!(json_to_frame(r#"{"type":4,"widget":{"kind":9}}"#).is_err());
        assert!(json_to_frame(
            r#"{"type":4,"widget":{"kind":2,"items":["a","b"],"tree":[[0,0]]}}"#
        )
        .is_err());
        assert!(
            json_to_frame(r#"{"type":4,"widget":{"kind":1,"items":["a"],"tree":[[0,0]]}}"#)
                .is_err()
        );
    }

    #[test]
    fn malformed_lines_are_rejected() {
        for bad in [
            "[]",
            r#"{"type":70000}"#,
            r#"{"type":1,"hex":"0"}"#,
            r#"{"type":1,"hex":"zz"}"#,
            r#"{"type":1,"text":"a","hex":"00"}"#,
            r#"{"type":1} x"#,
            r#"{"type":-1}"#,
        ] {
            assert!(json_to_frame(bad).is_err(), "{bad}");
        }
    }
}
