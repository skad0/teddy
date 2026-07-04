#[path = "../lex.rs"]
mod lex;
#[path = "../plugin.rs"]
mod plugin;

use lex::{Span, Tok};
use plugin::{encode, Frame, FrameReader, HELLO, PROTO_VERSION, SPANS, VIEWPORT};
use std::io::{self, Read, Write};

fn main() -> io::Result<()> {
    let mut reader = FrameReader::new();
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 8192];
    let mut spans = Vec::new();

    loop {
        match stdin.read(&mut buf)? {
            0 => return Ok(()),
            n => reader.push(&buf[..n]),
        }

        while let Some(frame) = reader.next().map_err(proto_io)? {
            match frame.msg_type {
                HELLO => {
                    if frame.payload.len() == 4
                        && u32::from_le_bytes(frame.payload[0..4].try_into().unwrap())
                            == PROTO_VERSION
                    {
                        write_frame(
                            &mut stdout,
                            &Frame {
                                msg_type: HELLO,
                                flags: 0x1,
                                request_id: 0,
                                resource_id: 0,
                                resource_revision: 0,
                                payload: PROTO_VERSION.to_le_bytes().to_vec(),
                            },
                        )?;
                    }
                }
                VIEWPORT => {
                    let Some((name, rows)) = parse_viewport(&frame.payload) else {
                        continue;
                    };
                    let Some(kind) = HighlightKind::for_name(name) else {
                        continue;
                    };
                    let payload = spans_payload(kind, rows, &mut spans);
                    write_frame(
                        &mut stdout,
                        &Frame {
                            msg_type: SPANS,
                            flags: 0,
                            request_id: frame.request_id,
                            resource_id: frame.resource_id,
                            resource_revision: frame.resource_revision,
                            payload,
                        },
                    )?;
                }
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy)]
enum HighlightKind {
    Rust,
    Markdown,
}

impl HighlightKind {
    fn for_name(name: &str) -> Option<Self> {
        if name.ends_with(".rs") {
            Some(Self::Rust)
        } else if name.ends_with(".md") || name.ends_with(".markdown") {
            Some(Self::Markdown)
        } else {
            None
        }
    }
}

fn parse_viewport(payload: &[u8]) -> Option<(&str, Vec<&[u8]>)> {
    if payload.len() < 4 {
        return None;
    }
    let name_len = u16::from_le_bytes(payload[0..2].try_into().ok()?) as usize;
    let mut at = 2usize;
    if at + name_len + 2 > payload.len() {
        return None;
    }
    let name = std::str::from_utf8(&payload[at..at + name_len]).ok()?;
    at += name_len;
    let row_count = u16::from_le_bytes(payload[at..at + 2].try_into().ok()?) as usize;
    at += 2;
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        if at + 2 > payload.len() {
            return None;
        }
        let len = u16::from_le_bytes(payload[at..at + 2].try_into().ok()?) as usize;
        at += 2;
        if at + len > payload.len() {
            return None;
        }
        rows.push(&payload[at..at + len]);
        at += len;
    }
    if at != payload.len() {
        return None;
    }
    Some((name, rows))
}

fn spans_payload(kind: HighlightKind, rows: Vec<&[u8]>, scratch: &mut Vec<Span>) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(rows.len() as u16).to_le_bytes());
    for (row_idx, row) in rows.into_iter().enumerate() {
        scratch.clear();
        match kind {
            HighlightKind::Rust => lex::lex_rust_line(row, scratch),
            HighlightKind::Markdown => lex::lex_markdown_line(row, scratch),
        }
        let span_count = scratch.iter().filter(|s| style_for(s.tok) != 0).count();
        payload.extend_from_slice(&(row_idx as u16).to_le_bytes());
        payload.extend_from_slice(&(span_count as u16).to_le_bytes());
        for span in scratch.iter().filter(|s| style_for(s.tok) != 0) {
            let start = span.start.min(u16::MAX as usize) as u16;
            let end = span.end.min(u16::MAX as usize);
            let len = end.saturating_sub(start as usize) as u16;
            if len == 0 {
                continue;
            }
            payload.extend_from_slice(&start.to_le_bytes());
            payload.extend_from_slice(&len.to_le_bytes());
            payload.push(style_for(span.tok));
        }
    }
    payload
}

fn style_for(tok: Tok) -> u8 {
    match tok {
        Tok::Keyword => 1,
        Tok::Str => 2,
        Tok::Comment => 3,
        Tok::Number => 4,
        Tok::Punct => 5,
        Tok::Heading => 6,
        Tok::Emphasis => 7,
        Tok::CodeSpan | Tok::Link => 8,
        Tok::Ident | Tok::Text => 0,
    }
}

fn write_frame<W: Write>(out: &mut W, frame: &Frame) -> io::Result<()> {
    let mut bytes = Vec::new();
    encode(frame, &mut bytes);
    out.write_all(&bytes)?;
    out.flush()
}

fn proto_io(e: plugin::ProtoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}"))
}
