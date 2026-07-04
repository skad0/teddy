use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const PROTO_VERSION: u32 = 1;
pub const MAX_PAYLOAD: u32 = 1 << 20;

pub const HELLO: u16 = 1;
pub const REGISTER_COMMAND: u16 = 2;
pub const COMMAND_INVOKE: u16 = 3;
pub const WIDGET: u16 = 4;
pub const WIDGET_EVENT: u16 = 5;
pub const EDIT_TX: u16 = 6;
pub const EDIT_RESULT: u16 = 7;
pub const STATUS: u16 = 8;

const HEADER_LEN: usize = 28;
// Per-tick pipe budget: a flooding plugin yields to input/paint and is polled again next tick.
const PUMP_READ_BUDGET: usize = 256 * 1024;
#[allow(dead_code)]
const NOTICE_CAP: usize = 8;

pub struct Frame {
    pub msg_type: u16,
    pub flags: u16,
    pub request_id: u32,
    pub resource_id: u64,
    pub resource_revision: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    Oversized,
    Malformed,
}

pub fn encode(f: &Frame, out: &mut Vec<u8>) {
    let len = f.payload.len();
    assert!(
        len <= MAX_PAYLOAD as usize,
        "plugin frame payload too large"
    );
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&f.msg_type.to_le_bytes());
    out.extend_from_slice(&f.flags.to_le_bytes());
    out.extend_from_slice(&f.request_id.to_le_bytes());
    out.extend_from_slice(&f.resource_id.to_le_bytes());
    out.extend_from_slice(&f.resource_revision.to_le_bytes());
    out.extend_from_slice(&f.payload);
}

pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> FrameReader {
        FrameReader { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn next(&mut self) -> Result<Option<Frame>, ProtoError> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }

        let payload_len = u32::from_le_bytes(take4(&self.buf[0..4])?);
        if payload_len > MAX_PAYLOAD {
            return Err(ProtoError::Oversized);
        }

        let end = HEADER_LEN
            .checked_add(payload_len as usize)
            .ok_or(ProtoError::Malformed)?;
        if self.buf.len() < end {
            return Ok(None);
        }

        let frame = Frame {
            msg_type: u16::from_le_bytes(take2(&self.buf[4..6])?),
            flags: u16::from_le_bytes(take2(&self.buf[6..8])?),
            request_id: u32::from_le_bytes(take4(&self.buf[8..12])?),
            resource_id: u64::from_le_bytes(take8(&self.buf[12..20])?),
            resource_revision: u64::from_le_bytes(take8(&self.buf[20..28])?),
            payload: self.buf[HEADER_LEN..end].to_vec(),
        };
        self.buf.drain(0..end);
        Ok(Some(frame))
    }
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

fn take2(bytes: &[u8]) -> Result<[u8; 2], ProtoError> {
    bytes.try_into().map_err(|_| ProtoError::Malformed)
}

fn take4(bytes: &[u8]) -> Result<[u8; 4], ProtoError> {
    bytes.try_into().map_err(|_| ProtoError::Malformed)
}

fn take8(bytes: &[u8]) -> Result<[u8; 8], ProtoError> {
    bytes.try_into().map_err(|_| ProtoError::Malformed)
}

#[allow(dead_code)]
pub struct Widget {
    pub kind: u8,
    pub items: Vec<String>,
    pub revision: u64,
}

#[allow(dead_code)]
pub fn parse_widget(payload: &[u8]) -> Option<Widget> {
    if payload.len() < 3 {
        return None;
    }

    let kind = payload[0];
    let count = u16::from_le_bytes(payload[1..3].try_into().ok()?) as usize;
    let mut at = 3;
    let mut items = Vec::with_capacity(count);

    for _ in 0..count {
        if at + 2 > payload.len() {
            return None;
        }
        let len = u16::from_le_bytes(payload[at..at + 2].try_into().ok()?) as usize;
        at += 2;
        if at + len > payload.len() {
            return None;
        }
        let item = std::str::from_utf8(&payload[at..at + len])
            .ok()?
            .to_string();
        items.push(sanitize_control_bytes(item));
        at += len;
    }

    if at != payload.len() {
        return None;
    }

    Some(Widget {
        kind,
        items,
        revision: 0,
    })
}

#[allow(dead_code)]
pub struct Plugin {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    reader: FrameReader,
    pub name: String,
    pub path: PathBuf,
    pub alive: bool,
    pub commands: Vec<String>,
    pub widgets: HashMap<u64, Widget>,
    pub notices: Vec<String>,
    hello_ok: bool,
}

#[allow(dead_code)]
impl Plugin {
    pub fn spawn(path: &Path) -> io::Result<Plugin> {
        let (child, mut stdin, stdout) = start_child(path)?;
        send_frame_to(
            &mut stdin,
            &Frame {
                msg_type: HELLO,
                flags: 0,
                request_id: 0,
                resource_id: 0,
                resource_revision: 0,
                payload: PROTO_VERSION.to_le_bytes().to_vec(),
            },
        )?;

        Ok(Plugin {
            child,
            stdin,
            stdout,
            reader: FrameReader::new(),
            name: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string(),
            path: path.to_path_buf(),
            alive: true,
            commands: Vec::new(),
            widgets: HashMap::new(),
            notices: Vec::new(),
            hello_ok: false,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.stdout.as_fd()
    }

    pub fn send(&mut self, f: &Frame) {
        if send_frame_to(&mut self.stdin, f).is_err() {
            self.stop_child();
        }
    }

    pub fn pump(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let mut read_total = 0usize;

        while read_total < PUMP_READ_BUDGET {
            let read_cap = (PUMP_READ_BUDGET - read_total).min(buf.len());
            match self.stdout.read(&mut buf[..read_cap]) {
                Ok(0) => {
                    self.stop_child();
                    break;
                }
                Ok(n) => {
                    read_total += n;
                    self.reader.push(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.stop_child();
                    break;
                }
            }

            loop {
                match self.reader.next() {
                    Ok(Some(frame)) => {
                        if !self.hello_ok {
                            if frame.msg_type != HELLO
                                || frame.payload.len() != 4
                                || u32::from_le_bytes(frame.payload[0..4].try_into().unwrap())
                                    != PROTO_VERSION
                            {
                                self.stop_child();
                                break;
                            }
                            self.hello_ok = true;
                            continue;
                        }
                        self.handle_frame(frame, &mut out);
                        if !self.alive {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        self.stop_child();
                        break;
                    }
                }
            }

            if !self.alive {
                break;
            }
        }

        out
    }

    pub fn restart(&mut self) -> io::Result<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.alive = false;

        let (child, mut stdin, stdout) = start_child(&self.path)?;
        send_frame_to(
            &mut stdin,
            &Frame {
                msg_type: HELLO,
                flags: 0,
                request_id: 0,
                resource_id: 0,
                resource_revision: 0,
                payload: PROTO_VERSION.to_le_bytes().to_vec(),
            },
        )?;

        self.child = child;
        self.stdin = stdin;
        self.stdout = stdout;
        self.reader = FrameReader::new();
        self.alive = true;
        self.commands.clear();
        self.widgets.clear();
        self.notices.clear();
        self.hello_ok = false;
        Ok(())
    }

    fn handle_frame(&mut self, frame: Frame, out: &mut Vec<Frame>) {
        match frame.msg_type {
            REGISTER_COMMAND => {
                if let Ok(command) = String::from_utf8(frame.payload) {
                    self.commands.push(command);
                } else {
                    self.stop_child();
                }
            }
            WIDGET => {
                if !update_widget(
                    &mut self.widgets,
                    frame.resource_id,
                    frame.resource_revision,
                    &frame.payload,
                ) {
                    self.stop_child();
                }
            }
            STATUS => {
                if let Ok(notice) = String::from_utf8(frame.payload) {
                    if self.notices.len() == NOTICE_CAP {
                        self.notices.remove(0);
                    }
                    self.notices.push(sanitize_control_bytes(notice));
                } else {
                    self.stop_child();
                }
            }
            _ => out.push(frame),
        }
    }

    fn stop_child(&mut self) {
        self.alive = false;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(dead_code)]
fn start_child(path: &Path) -> io::Result<(Child, ChildStdin, ChildStdout)> {
    let mut child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "missing plugin stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "missing plugin stdout"))?;
    let flags = fcntl_getfl(&stdout)?;
    fcntl_setfl(&stdout, flags | OFlags::NONBLOCK)?;
    let flags = fcntl_getfl(&stdin)?;
    fcntl_setfl(&stdin, flags | OFlags::NONBLOCK)?;
    Ok((child, stdin, stdout))
}

#[allow(dead_code)]
fn send_frame_to(stdin: &mut ChildStdin, f: &Frame) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + f.payload.len());
    encode(f, &mut bytes);
    // No outbox buffering: plugins must drain stdin or they are killed on backpressure.
    // Buffered writes can be added if a real plugin needs them.
    stdin.write_all(&bytes)
}

fn sanitize_control_bytes(s: String) -> String {
    s.chars()
        .map(|c| if c < ' ' || c == '\u{7f}' { '?' } else { c })
        .collect()
}

#[allow(dead_code)]
fn update_widget(
    widgets: &mut HashMap<u64, Widget>,
    id: u64,
    revision: u64,
    payload: &[u8],
) -> bool {
    let Some(mut widget) = parse_widget(payload) else {
        return false;
    };
    if widgets.get(&id).map_or(true, |old| revision > old.revision) {
        widget.revision = revision;
        widgets.insert(id, widget);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &[u8]) -> Frame {
        Frame {
            msg_type: STATUS,
            flags: 3,
            request_id: 7,
            resource_id: 11,
            resource_revision: 13,
            payload: payload.to_vec(),
        }
    }

    fn widget_payload(items: &[&str]) -> Vec<u8> {
        let mut payload = vec![1];
        payload.extend_from_slice(&(items.len() as u16).to_le_bytes());
        for item in items {
            payload.extend_from_slice(&(item.len() as u16).to_le_bytes());
            payload.extend_from_slice(item.as_bytes());
        }
        payload
    }

    #[test]
    fn round_trip_split_boundaries() {
        let original = frame(b"hello");
        let mut bytes = Vec::new();
        encode(&original, &mut bytes);

        let mut reader = FrameReader::new();
        reader.push(&bytes[..5]);
        assert!(reader.next().unwrap().is_none());
        reader.push(&bytes[5..HEADER_LEN + 2]);
        assert!(reader.next().unwrap().is_none());
        reader.push(&bytes[HEADER_LEN + 2..]);

        let got = reader.next().unwrap().unwrap();
        assert_eq!(got.msg_type, original.msg_type);
        assert_eq!(got.flags, original.flags);
        assert_eq!(got.request_id, original.request_id);
        assert_eq!(got.resource_id, original.resource_id);
        assert_eq!(got.resource_revision, original.resource_revision);
        assert_eq!(got.payload, original.payload);
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn oversized_payload_len_rejected() {
        let mut reader = FrameReader::new();
        let mut header = vec![0u8; HEADER_LEN];
        header[..4].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        reader.push(&header);
        assert!(matches!(reader.next(), Err(ProtoError::Oversized)));
    }

    #[test]
    fn parses_widget_and_rejects_truncated_or_overlong() {
        let payload = widget_payload(&["one", "two"]);
        let widget = parse_widget(&payload).unwrap();
        assert_eq!(widget.kind, 1);
        assert_eq!(widget.items, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(widget.revision, 0);

        assert!(parse_widget(&payload[..payload.len() - 1]).is_none());

        let mut overlong = payload;
        overlong.push(0);
        assert!(parse_widget(&overlong).is_none());
    }

    #[test]
    fn sanitizer_replaces_control_bytes() {
        assert_eq!(
            sanitize_control_bytes("ok\tline\n\r\u{1b}\u{7f}\u{80}".to_string()),
            "ok?line????\u{80}"
        );

        let payload = widget_payload(&["a\tb\u{7f}"]);
        let widget = parse_widget(&payload).unwrap();
        assert_eq!(widget.items, vec!["a?b?".to_string()]);
    }

    #[test]
    fn stale_widget_revision_is_dropped() {
        let mut widgets = HashMap::new();
        assert!(update_widget(&mut widgets, 1, 2, &widget_payload(&["new"])));
        assert_eq!(widgets.get(&1).unwrap().items, vec!["new".to_string()]);

        assert!(update_widget(
            &mut widgets,
            1,
            1,
            &widget_payload(&["stale"])
        ));
        assert_eq!(widgets.get(&1).unwrap().revision, 2);
        assert_eq!(widgets.get(&1).unwrap().items, vec!["new".to_string()]);

        assert!(update_widget(
            &mut widgets,
            1,
            3,
            &widget_payload(&["newer"])
        ));
        assert_eq!(widgets.get(&1).unwrap().revision, 3);
        assert_eq!(widgets.get(&1).unwrap().items, vec!["newer".to_string()]);
    }
}
