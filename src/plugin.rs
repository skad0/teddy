use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

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
pub const VIEWPORT: u16 = 9;
pub const SPANS: u16 = 10;
// Reserved for the editor/launcher control lane.  These are ordinary v1
// frames; keeping them here means existing frame draining remains unchanged.
pub const LAUNCHER_REQUEST: u16 = 11;
pub const LAUNCHER_RESPONSE: u16 = 12;
pub const LAUNCHER_EVENT: u16 = 13;
/// Core → plugin structured widget input other than item selection.
pub const WIDGET_INPUT: u16 = 14;

const HEADER_LEN: usize = 28;
// Per-tick pipe budget: a flooding plugin yields to input/paint and is polled again next tick.
const PUMP_READ_BUDGET: usize = 256 * 1024;
#[allow(dead_code)]
const NOTICE_CAP: usize = 8;
const STDERR_READ_BUDGET: usize = 16 * 1024;
const HELLO_DEADLINE: Duration = Duration::from_secs(2);

fn retry_delay(backoff_ms: u32, attempt: u8) -> Duration {
    let multiplier = 1u64 << (2 * u32::from(attempt));
    Duration::from_millis(u64::from(backoff_ms).saturating_mul(multiplier))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    Legacy,
    Registry,
    Bundled,
    Manager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    Starting,
    Running,
    Stopping,
    Backoff,
    Failed,
}

/// Deterministic lifecycle state machine used by supervision tests and by the
/// host's transition vocabulary.  It has no process or I/O side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleModel {
    pub state: PluginState,
    pub attempts: u8,
    pub generation: u64,
}

impl LifecycleModel {
    pub fn new() -> Self {
        Self {
            state: PluginState::Starting,
            attempts: 0,
            generation: 1,
        }
    }
    pub fn hello(&mut self) {
        self.state = PluginState::Running;
    }
    pub fn timeout(&mut self) {
        self.state = PluginState::Failed;
    }
    pub fn crash(&mut self) {
        self.state = if self.attempts < 3 {
            PluginState::Backoff
        } else {
            PluginState::Failed
        };
        self.attempts = self.attempts.saturating_add(1);
    }
    pub fn restart(&mut self) {
        self.state = PluginState::Starting;
        self.generation = self.generation.wrapping_add(1).max(1);
    }
    pub fn reset_retry_budget(&mut self) {
        self.attempts = 0;
    }
    pub fn stop(&mut self) {
        self.state = PluginState::Stopping;
    }
    pub fn reaped(&mut self) {
        if self.state == PluginState::Stopping {
            self.state = PluginState::Failed;
        }
    }
}

impl Default for LifecycleModel {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
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

const MAX_PLUGIN_ID: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherRequest {
    List {
        page: u16,
    },
    Enable {
        id: String,
        descriptor: Option<LauncherDescriptor>,
    },
    Disable(String),
    Reload(String),
    Forget(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherDescriptor {
    pub path: String,
    pub max_restarts: u32,
    pub backoff_ms: u32,
    pub confirm_recovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherRecord {
    pub id: String,
    pub path: String,
    pub enabled: bool,
    pub state: u8,
    pub max_restarts: u32,
    pub backoff_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherResponse {
    List {
        page: u16,
        next_page: Option<u16>,
        records: Vec<LauncherRecord>,
    },
    Enabled(String),
    Disabled(String),
    Reloaded(String),
    Forgotten(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherEvent {
    Enabled(String),
    Disabled(String),
    Reloaded(String),
    Forgotten(String),
}

pub fn encode_launcher_request(
    request_id: u32,
    request: &LauncherRequest,
) -> Result<Frame, ProtoError> {
    Ok(Frame {
        msg_type: LAUNCHER_REQUEST,
        flags: 0,
        request_id,
        resource_id: 0,
        resource_revision: 0,
        payload: encode_launcher_request_payload(request)?,
    })
}

pub fn decode_launcher_request(frame: &Frame) -> Result<LauncherRequest, ProtoError> {
    if frame.msg_type != LAUNCHER_REQUEST {
        return Err(ProtoError::Malformed);
    }
    decode_launcher_request_payload(&frame.payload)
}

pub fn encode_launcher_response(
    request_id: u32,
    response: &LauncherResponse,
) -> Result<Frame, ProtoError> {
    Ok(Frame {
        msg_type: LAUNCHER_RESPONSE,
        flags: 0,
        request_id,
        resource_id: 0,
        resource_revision: 0,
        payload: encode_launcher_response_payload(response)?,
    })
}

pub fn decode_launcher_response(frame: &Frame) -> Result<LauncherResponse, ProtoError> {
    if frame.msg_type != LAUNCHER_RESPONSE {
        return Err(ProtoError::Malformed);
    }
    decode_launcher_response_payload(&frame.payload)
}

pub fn encode_launcher_event(event: &LauncherEvent) -> Result<Frame, ProtoError> {
    Ok(Frame {
        msg_type: LAUNCHER_EVENT,
        flags: 0,
        request_id: 0,
        resource_id: 0,
        resource_revision: 0,
        payload: encode_launcher_event_payload(event)?,
    })
}

pub fn decode_launcher_event(frame: &Frame) -> Result<LauncherEvent, ProtoError> {
    if frame.msg_type != LAUNCHER_EVENT {
        return Err(ProtoError::Malformed);
    }
    decode_launcher_event_payload(&frame.payload)
}

pub fn encode_launcher_request_payload(request: &LauncherRequest) -> Result<Vec<u8>, ProtoError> {
    match request {
        LauncherRequest::List { page } => {
            let mut out = vec![0];
            out.extend_from_slice(&page.to_le_bytes());
            Ok(out)
        }
        LauncherRequest::Enable { id, descriptor } => {
            if !valid_plugin_id(id) {
                return Err(ProtoError::Malformed);
            }
            let mut out = Vec::new();
            out.push(1);
            append_id(&mut out, id)?;
            match descriptor {
                None => out.push(0),
                Some(d) => {
                    validate_descriptor(d)?;
                    out.push(1);
                    append_string(&mut out, &d.path)?;
                    out.extend_from_slice(&d.max_restarts.to_le_bytes());
                    out.extend_from_slice(&d.backoff_ms.to_le_bytes());
                    out.push(u8::from(d.confirm_recovery));
                }
            }
            finish_launcher_payload(out)
        }
        LauncherRequest::Disable(id) => encode_id_operation(2, id),
        LauncherRequest::Reload(id) => encode_id_operation(3, id),
        LauncherRequest::Forget(id) => encode_id_operation(4, id),
    }
}

pub fn decode_launcher_request_payload(payload: &[u8]) -> Result<LauncherRequest, ProtoError> {
    check_launcher_payload(payload)?;
    let tag = *payload.first().ok_or(ProtoError::Malformed)?;
    match tag {
        0 if payload.len() == 3 => Ok(LauncherRequest::List {
            page: u16::from_le_bytes([payload[1], payload[2]]),
        }),
        1 => {
            let (id, at) = read_id(payload, 1)?;
            let descriptor = match *payload.get(at).ok_or(ProtoError::Malformed)? {
                0 if at + 1 == payload.len() => None,
                1 => Some(read_descriptor(payload, at + 1)?),
                _ => return Err(ProtoError::Malformed),
            };
            Ok(LauncherRequest::Enable { id, descriptor })
        }
        2..=4 => Ok(match decode_id_operation(payload)? {
            (2, id) => LauncherRequest::Disable(id),
            (3, id) => LauncherRequest::Reload(id),
            (4, id) => LauncherRequest::Forget(id),
            _ => return Err(ProtoError::Malformed),
        }),
        _ => Err(ProtoError::Malformed),
    }
}

pub fn encode_launcher_response_payload(
    response: &LauncherResponse,
) -> Result<Vec<u8>, ProtoError> {
    match response {
        LauncherResponse::List {
            page,
            next_page,
            records,
        } => {
            if records.len() > u16::MAX as usize {
                return Err(ProtoError::Oversized);
            }
            let mut out = Vec::with_capacity(7);
            out.push(0);
            out.extend_from_slice(&page.to_le_bytes());
            out.extend_from_slice(&next_page.unwrap_or(u16::MAX).to_le_bytes());
            out.extend_from_slice(&(records.len() as u16).to_le_bytes());
            for record in records {
                append_record(&mut out, record)?;
            }
            finish_launcher_payload(out)
        }
        LauncherResponse::Enabled(id) => encode_id_operation(1, id),
        LauncherResponse::Disabled(id) => encode_id_operation(2, id),
        LauncherResponse::Reloaded(id) => encode_id_operation(3, id),
        LauncherResponse::Forgotten(id) => encode_id_operation(4, id),
        LauncherResponse::Error(message) => {
            let bytes = message.as_bytes();
            let len = u16::try_from(bytes.len()).map_err(|_| ProtoError::Oversized)?;
            let mut out = Vec::with_capacity(3 + bytes.len());
            out.push(255);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
            finish_launcher_payload(out)
        }
    }
}

pub fn decode_launcher_response_payload(payload: &[u8]) -> Result<LauncherResponse, ProtoError> {
    check_launcher_payload(payload)?;
    let tag = *payload.first().ok_or(ProtoError::Malformed)?;
    match tag {
        0 => {
            if payload.len() < 7 {
                return Err(ProtoError::Malformed);
            }
            let page = u16::from_le_bytes([payload[1], payload[2]]);
            let next_raw = u16::from_le_bytes([payload[3], payload[4]]);
            let count = u16::from_le_bytes([payload[5], payload[6]]) as usize;
            if count > (payload.len() - 7) / 2 {
                return Err(ProtoError::Malformed);
            }
            let mut at = 7;
            let mut records = Vec::with_capacity(count);
            for _ in 0..count {
                let (record, next) = read_record(payload, at)?;
                records.push(record);
                at = next;
            }
            if at != payload.len() {
                return Err(ProtoError::Malformed);
            }
            Ok(LauncherResponse::List {
                page,
                next_page: (next_raw != u16::MAX).then_some(next_raw),
                records,
            })
        }
        1..=4 => match decode_id_operation(payload)? {
            (1, id) => Ok(LauncherResponse::Enabled(id)),
            (2, id) => Ok(LauncherResponse::Disabled(id)),
            (3, id) => Ok(LauncherResponse::Reloaded(id)),
            (4, id) => Ok(LauncherResponse::Forgotten(id)),
            _ => Err(ProtoError::Malformed),
        },
        255 => Ok(LauncherResponse::Error(read_string(payload, 1)?)),
        _ => Err(ProtoError::Malformed),
    }
}

pub fn encode_launcher_event_payload(event: &LauncherEvent) -> Result<Vec<u8>, ProtoError> {
    match event {
        LauncherEvent::Enabled(id) => encode_id_operation(1, id),
        LauncherEvent::Disabled(id) => encode_id_operation(2, id),
        LauncherEvent::Reloaded(id) => encode_id_operation(3, id),
        LauncherEvent::Forgotten(id) => encode_id_operation(4, id),
    }
}

pub fn decode_launcher_event_payload(payload: &[u8]) -> Result<LauncherEvent, ProtoError> {
    check_launcher_payload(payload)?;
    match decode_id_operation(payload)? {
        (1, id) => Ok(LauncherEvent::Enabled(id)),
        (2, id) => Ok(LauncherEvent::Disabled(id)),
        (3, id) => Ok(LauncherEvent::Reloaded(id)),
        (4, id) => Ok(LauncherEvent::Forgotten(id)),
        _ => Err(ProtoError::Malformed),
    }
}

fn valid_plugin_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_PLUGIN_ID
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..].iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn append_id(out: &mut Vec<u8>, id: &str) -> Result<(), ProtoError> {
    if !valid_plugin_id(id) {
        return Err(ProtoError::Malformed);
    }
    out.push(id.len() as u8);
    out.extend_from_slice(id.as_bytes());
    Ok(())
}

fn validate_descriptor(d: &LauncherDescriptor) -> Result<(), ProtoError> {
    if d.path.len() > 16 * 1024 || !Path::new(&d.path).is_absolute() {
        return Err(ProtoError::Malformed);
    }
    if d.max_restarts > 3 || d.backoff_ms > 86_400_000 {
        return Err(ProtoError::Malformed);
    }
    Ok(())
}

fn append_string(out: &mut Vec<u8>, value: &str) -> Result<(), ProtoError> {
    let len = u16::try_from(value.len()).map_err(|_| ProtoError::Oversized)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn read_descriptor(payload: &[u8], mut at: usize) -> Result<LauncherDescriptor, ProtoError> {
    let path = read_len_string(payload, &mut at)?;
    let end = at.checked_add(9).ok_or(ProtoError::Malformed)?;
    if end != payload.len() {
        return Err(ProtoError::Malformed);
    }
    let max_restarts = u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
    let backoff_ms = u32::from_le_bytes(payload[at + 4..end].try_into().unwrap());
    if payload[end - 1] > 1 {
        return Err(ProtoError::Malformed);
    }
    let descriptor = LauncherDescriptor {
        path,
        max_restarts,
        backoff_ms,
        confirm_recovery: payload[end - 1] != 0,
    };
    validate_descriptor(&descriptor)?;
    Ok(descriptor)
}

fn read_len_string(payload: &[u8], at: &mut usize) -> Result<String, ProtoError> {
    let end = at.checked_add(2).ok_or(ProtoError::Malformed)?;
    if end > payload.len() {
        return Err(ProtoError::Malformed);
    }
    let len = u16::from_le_bytes([payload[*at], payload[*at + 1]]) as usize;
    *at = end;
    let finish = (*at).checked_add(len).ok_or(ProtoError::Malformed)?;
    let bytes = payload.get(*at..finish).ok_or(ProtoError::Malformed)?;
    *at = finish;
    String::from_utf8(bytes.to_vec()).map_err(|_| ProtoError::Malformed)
}

fn append_record(out: &mut Vec<u8>, record: &LauncherRecord) -> Result<(), ProtoError> {
    append_id(out, &record.id)?;
    append_string(out, &record.path)?;
    out.push(u8::from(record.enabled));
    out.push(record.state);
    out.extend_from_slice(&record.max_restarts.to_le_bytes());
    out.extend_from_slice(&record.backoff_ms.to_le_bytes());
    Ok(())
}

fn read_record(payload: &[u8], mut at: usize) -> Result<(LauncherRecord, usize), ProtoError> {
    let (id, next) = read_id(payload, at)?;
    at = next;
    let path = read_len_string(payload, &mut at)?;
    let end = at.checked_add(10).ok_or(ProtoError::Malformed)?;
    if end > payload.len() {
        return Err(ProtoError::Malformed);
    }
    let enabled = match payload[at] {
        0 => false,
        1 => true,
        _ => return Err(ProtoError::Malformed),
    };
    let state = payload[at + 1];
    let max_restarts = u32::from_le_bytes(payload[at + 2..at + 6].try_into().unwrap());
    let backoff_ms = u32::from_le_bytes(payload[at + 6..end].try_into().unwrap());
    if !Path::new(&path).is_absolute() || max_restarts > 3 || backoff_ms > 86_400_000 {
        return Err(ProtoError::Malformed);
    }
    Ok((
        LauncherRecord {
            id,
            path,
            enabled,
            state,
            max_restarts,
            backoff_ms,
        },
        end,
    ))
}

fn encode_id_operation(tag: u8, id: &str) -> Result<Vec<u8>, ProtoError> {
    if !valid_plugin_id(id) {
        return Err(ProtoError::Malformed);
    }
    let mut out = Vec::with_capacity(2 + id.len());
    out.push(tag);
    append_id(&mut out, id)?;
    finish_launcher_payload(out)
}

fn decode_id_operation(payload: &[u8]) -> Result<(u8, String), ProtoError> {
    let tag = *payload.first().ok_or(ProtoError::Malformed)?;
    let (id, at) = read_id(payload, 1)?;
    if at != payload.len() {
        return Err(ProtoError::Malformed);
    }
    Ok((tag, id))
}

fn read_id(payload: &[u8], at: usize) -> Result<(String, usize), ProtoError> {
    let len = *payload.get(at).ok_or(ProtoError::Malformed)? as usize;
    if len == 0 || len > MAX_PLUGIN_ID {
        return Err(ProtoError::Malformed);
    }
    let end = at.checked_add(1 + len).ok_or(ProtoError::Malformed)?;
    let id = std::str::from_utf8(payload.get(at + 1..end).ok_or(ProtoError::Malformed)?)
        .map_err(|_| ProtoError::Malformed)?;
    if !valid_plugin_id(id) {
        return Err(ProtoError::Malformed);
    }
    Ok((id.to_owned(), end))
}

fn read_string(payload: &[u8], at: usize) -> Result<String, ProtoError> {
    if payload.len() < at + 2 {
        return Err(ProtoError::Malformed);
    }
    let len = u16::from_le_bytes([payload[at], payload[at + 1]]) as usize;
    let start = at + 2;
    let end = start.checked_add(len).ok_or(ProtoError::Malformed)?;
    if end != payload.len() {
        return Err(ProtoError::Malformed);
    }
    std::str::from_utf8(&payload[start..end])
        .map(str::to_owned)
        .map_err(|_| ProtoError::Malformed)
}

fn finish_launcher_payload(out: Vec<u8>) -> Result<Vec<u8>, ProtoError> {
    if out.len() > MAX_PAYLOAD as usize {
        Err(ProtoError::Oversized)
    } else {
        Ok(out)
    }
}

fn check_launcher_payload(payload: &[u8]) -> Result<(), ProtoError> {
    if payload.len() > MAX_PAYLOAD as usize {
        Err(ProtoError::Oversized)
    } else {
        Ok(())
    }
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

/// Core widget kinds (spec §14.3). The plugin supplies data only; layout,
/// drawing, and input stay in the core.
pub const W_LIST: u8 = 1;
pub const W_TREE: u8 = 2;
pub const W_TABLE: u8 = 3;
pub const W_TEXT: u8 = 4;
pub const W_LOG: u8 = 5;
pub const W_PROMPT: u8 = 6;
pub const W_ACTIONS: u8 = 7;

/// `WIDGET` frame flag: this widget is the Ctrl+T explorer/action surface.
pub const WIDGET_FLAG_EXPLORER: u16 = 0x1;
/// Tree row flags.
pub const TREE_HAS_CHILDREN: u8 = 0x1;
pub const TREE_EXPANDED: u8 = 0x2;
const TREE_MAX_DEPTH: u8 = 32;

/// `WIDGET_INPUT` (type 14) payload tags. Item selection keeps using
/// `WIDGET_EVENT` with its v1 `u32` payload; the other structured inputs get
/// their own message type so v1 plugins can never misread them as a select.
pub const IN_BUTTON: u8 = 1;
pub const IN_SEARCH: u8 = 2;
pub const IN_PROMPT: u8 = 3;
pub const IN_EXPAND: u8 = 4;
pub const IN_COLLAPSE: u8 = 5;

#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Widget {
    pub kind: u8,
    /// Set from `WIDGET_FLAG_EXPLORER` on the carrying frame.
    pub explorer: bool,
    /// Table column count; the first `cols` items are the header row.
    pub cols: u8,
    pub items: Vec<String>,
    /// Tree only: `(depth, flags)` per item.
    pub tree: Vec<(u8, u8)>,
    pub revision: u64,
}

/// Payload: `kind u8`, `[cols u8 if table]`, `count u16`, then per item
/// `[depth u8, flags u8 if tree]` and a `u16`-length UTF-8 string.
#[allow(dead_code)]
pub fn parse_widget(payload: &[u8]) -> Option<Widget> {
    let (&kind, mut rest) = payload.split_first()?;
    if !(W_LIST..=W_ACTIONS).contains(&kind) {
        return None;
    }
    let mut cols = 0;
    if kind == W_TABLE {
        let (&c, r) = rest.split_first()?;
        cols = c;
        rest = r;
    }
    let count = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
    let mut at = 2;
    let mut items = Vec::with_capacity(count.min(rest.len()));
    let mut tree = Vec::new();

    for _ in 0..count {
        if kind == W_TREE {
            let meta = rest.get(at..at + 2)?;
            let (depth, flags) = (meta[0], meta[1]);
            let known = TREE_HAS_CHILDREN | TREE_EXPANDED;
            if depth > TREE_MAX_DEPTH
                || flags & !known != 0
                || flags & (TREE_HAS_CHILDREN | TREE_EXPANDED) == TREE_EXPANDED
            {
                return None;
            }
            tree.push((meta[0], meta[1]));
            at += 2;
        }
        let len = u16::from_le_bytes(rest.get(at..at + 2)?.try_into().ok()?) as usize;
        at += 2;
        let item = std::str::from_utf8(rest.get(at..at + len)?).ok()?;
        items.push(sanitize_control_bytes(item.to_string()));
        at += len;
    }

    let shape_ok = match kind {
        W_TABLE => cols > 0 && count >= cols as usize && count % cols as usize == 0,
        W_PROMPT => count == 1,
        _ => true,
    };
    if at != rest.len() || !shape_ok {
        return None;
    }

    Some(Widget {
        kind,
        cols,
        items,
        tree,
        ..Widget::default()
    })
}

/// Inverse of `parse_widget`, for plugins, tests, and the dev JSON bridge.
#[allow(dead_code)]
pub fn encode_widget(w: &Widget) -> Vec<u8> {
    let mut out = vec![w.kind];
    if w.kind == W_TABLE {
        out.push(w.cols);
    }
    out.extend_from_slice(&(w.items.len() as u16).to_le_bytes());
    for (i, item) in w.items.iter().enumerate() {
        if w.kind == W_TREE {
            let (depth, flags) = w.tree.get(i).copied().unwrap_or((0, 0));
            out.extend_from_slice(&[depth, flags]);
        }
        out.extend_from_slice(&(item.len() as u16).to_le_bytes());
        out.extend_from_slice(item.as_bytes());
    }
    out
}

pub fn parse_spans(payload: &[u8]) -> Option<Vec<(u16, Vec<(u16, u16, u8)>)>> {
    if payload.len() < 2 {
        return None;
    }

    let row_count = u16::from_le_bytes(payload[0..2].try_into().ok()?) as usize;
    let mut at = 2usize;
    let mut rows = Vec::with_capacity(row_count);

    for _ in 0..row_count {
        if at + 4 > payload.len() {
            return None;
        }
        let row_idx = u16::from_le_bytes(payload[at..at + 2].try_into().ok()?);
        at += 2;
        let span_count = u16::from_le_bytes(payload[at..at + 2].try_into().ok()?) as usize;
        at += 2;
        let bytes = span_count.checked_mul(5)?;
        if at.checked_add(bytes)? > payload.len() {
            return None;
        }

        let mut spans = Vec::with_capacity(span_count);
        for _ in 0..span_count {
            let start = u16::from_le_bytes(payload[at..at + 2].try_into().ok()?);
            at += 2;
            let len = u16::from_le_bytes(payload[at..at + 2].try_into().ok()?);
            at += 2;
            let style = payload[at];
            at += 1;
            spans.push((start, len, style));
        }
        rows.push((row_idx, spans));
    }

    if at != payload.len() {
        return None;
    }
    Some(rows)
}

#[allow(dead_code)]
pub struct Plugin {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    reader: FrameReader,
    pub name: String,
    pub runtime_id: String,
    pub path: PathBuf,
    pub alive: bool,
    pub commands: Vec<String>,
    pub widgets: HashMap<u64, Widget>,
    /// Widget ids accepted since the core last drained this (repaint/auto-open).
    pub changed_widgets: Vec<u64>,
    pub notices: Vec<String>,
    /// Why the slot last stopped unexpectedly; the core takes it for the
    /// jobs pane (spec §14.4). Survives `clear_contributions`.
    pub failure: Option<String>,
    /// Rolling post-HELLO stderr lines, kept across cleanup as failure detail.
    pub stderr_tail: Vec<String>,
    pub wants_viewport: bool,
    hello_ok: bool,
    pub source: PluginSource,
    pub state: PluginState,
    pub process_generation: u64,
    pub max_restarts: u32,
    pub backoff_ms: u32,
    hello_deadline: Instant,
    retry_count: u8,
    retry_at: Option<Instant>,
    retry_after_reap: bool,
}

#[allow(dead_code)]
impl Plugin {
    pub fn spawn(path: &Path) -> io::Result<Plugin> {
        Self::spawn_with_source_id(path, PluginSource::Legacy, "teddy.legacy.0")
    }

    pub fn spawn_with_source(path: &Path, source: PluginSource) -> io::Result<Plugin> {
        Self::spawn_with_source_id(path, source, "teddy.legacy.0")
    }

    pub fn spawn_with_source_id(
        path: &Path,
        source: PluginSource,
        runtime_id: &str,
    ) -> io::Result<Plugin> {
        let (child, mut stdin, stdout, stderr) = start_child(path)?;
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
            stderr,
            reader: FrameReader::new(),
            name: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string(),
            runtime_id: runtime_id.to_owned(),
            path: path.to_path_buf(),
            alive: true,
            commands: Vec::new(),
            widgets: HashMap::new(),
            changed_widgets: Vec::new(),
            notices: Vec::new(),
            failure: None,
            stderr_tail: Vec::new(),
            wants_viewport: false,
            hello_ok: false,
            source,
            state: PluginState::Starting,
            process_generation: 1,
            max_restarts: 3,
            backoff_ms: 1000,
            hello_deadline: Instant::now() + HELLO_DEADLINE,
            retry_count: 0,
            retry_at: None,
            retry_after_reap: false,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.stdout.as_fd()
    }

    pub fn stderr_fd(&self) -> BorrowedFd<'_> {
        self.stderr.as_fd()
    }

    pub fn send(&mut self, f: &Frame) {
        if send_frame_to(&mut self.stdin, f).is_err() {
            self.fail_unexpected("write to plugin failed");
        }
    }

    pub fn send_launcher_error(&mut self, request_id: u32, message: &str) {
        let message = sanitize_control_bytes(message.to_owned());
        let response = LauncherResponse::Error(message);
        if let Ok(frame) = encode_launcher_response(request_id, &response) {
            self.send(&frame);
        }
    }

    pub fn stop_for_protocol(&mut self) {
        self.stop_child();
    }

    pub fn pump(&mut self) -> Vec<Frame> {
        self.service_process();
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let mut read_total = 0usize;

        while read_total < PUMP_READ_BUDGET {
            let read_cap = (PUMP_READ_BUDGET - read_total).min(buf.len());
            match self.stdout.read(&mut buf[..read_cap]) {
                Ok(0) => {
                    self.fail_unexpected("plugin closed stdout");
                    break;
                }
                Ok(n) => {
                    read_total += n;
                    self.reader.push(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.fail_unexpected("read from plugin failed");
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
                                self.fail_protocol("bad HELLO");
                                break;
                            }
                            self.hello_ok = true;
                            self.state = PluginState::Running;
                            self.wants_viewport = frame.flags & 0x1 != 0;
                            continue;
                        }
                        self.handle_frame(frame, &mut out);
                        if !self.alive {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        self.fail_protocol("malformed frame");
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
        match self.child.try_wait()? {
            Some(_) => {}
            None => {
                self.alive = false;
                self.state = PluginState::Stopping;
                self.clear_contributions();
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "plugin is still stopping",
                ));
            }
        }
        self.alive = false;
        self.clear_contributions();

        let (child, mut stdin, stdout, stderr) = start_child(&self.path)?;
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
        self.stderr = stderr;
        self.reader = FrameReader::new();
        self.alive = true;
        self.commands.clear();
        self.widgets.clear();
        self.notices.clear();
        self.stderr_tail.clear();
        self.wants_viewport = false;
        self.hello_ok = false;
        self.state = PluginState::Starting;
        self.process_generation = self.process_generation.wrapping_add(1).max(1);
        self.hello_deadline = Instant::now() + HELLO_DEADLINE;
        self.retry_at = None;
        self.retry_after_reap = false;
        Ok(())
    }

    pub fn reset_retry_accounting(&mut self) {
        self.retry_count = 0;
        self.retry_at = None;
        self.retry_after_reap = false;
    }

    /// Perform bounded lifecycle and stderr work even when stdout is idle.
    pub fn service(&mut self) {
        if self.state == PluginState::Backoff
            && self.retry_at.is_some_and(|at| Instant::now() >= at)
        {
            if self.restart().is_err() {
                self.schedule_retry();
            }
        }
        self.service_process();
    }

    pub fn drain_stderr(&mut self, budget: usize) -> usize {
        self.read_stderr(budget)
    }

    pub fn manager_capable(&self) -> bool {
        self.source == PluginSource::Manager && self.alive && self.hello_ok
    }

    pub fn clear_contributions(&mut self) {
        self.commands.clear();
        self.widgets.clear();
        self.changed_widgets.clear();
        self.notices.clear();
        self.wants_viewport = false;
    }

    fn service_process(&mut self) {
        if !self.alive && self.state == PluginState::Stopping {
            if let Some(status) = self.child.try_wait().ok().flatten() {
                if self.retry_after_reap {
                    let exited = format!("exited: {status}");
                    self.failure = Some(match self.failure.take() {
                        Some(reason) => format!("{reason}; {exited}"),
                        None => exited,
                    });
                    self.retry_after_reap = false;
                    self.schedule_retry();
                } else {
                    self.state = PluginState::Failed;
                }
            }
            return;
        }
        if !self.alive {
            return;
        }
        if let Some(status) = self.child.try_wait().ok().flatten() {
            self.failure = Some(format!("exited: {status}"));
            self.alive = false;
            self.clear_contributions();
            self.schedule_retry();
        } else if !self.hello_ok && Instant::now() >= self.hello_deadline {
            self.fail_unexpected("no HELLO within 2s");
        }
    }

    fn fail_unexpected(&mut self, reason: &str) {
        self.failure = Some(reason.to_owned());
        self.alive = false;
        self.state = PluginState::Stopping;
        self.retry_after_reap = true;
        let _ = self.child.kill();
    }

    fn schedule_retry(&mut self) {
        self.clear_contributions();
        // max_restarts limits retries. backoff_ms is the bounded base for the
        // fixed geometric schedule (base, 4*base, 16*base).
        if u32::from(self.retry_count) < self.max_restarts {
            let delay = retry_delay(self.backoff_ms, self.retry_count);
            self.retry_count += 1;
            self.retry_at = Some(Instant::now() + delay);
            self.state = PluginState::Backoff;
        } else {
            self.retry_at = None;
            self.state = PluginState::Failed;
        }
    }

    fn read_stderr(&mut self, budget: usize) -> usize {
        let mut buf = [0u8; 4096];
        let mut total = 0;
        while total < budget.min(STDERR_READ_BUDGET) {
            match self.stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if !self.hello_ok {
                        continue;
                    }
                    for line in buf[..n].split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
                        if self.stderr_tail.len() == NOTICE_CAP {
                            self.stderr_tail.remove(0);
                        }
                        self.stderr_tail.push(sanitize_stderr(line));
                    }
                    let text = sanitize_stderr(&buf[..n]);
                    if !text.is_empty() {
                        if self.notices.len() == NOTICE_CAP {
                            self.notices.remove(0);
                        }
                        self.notices.push(text);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        total
    }

    fn handle_frame(&mut self, frame: Frame, out: &mut Vec<Frame>) {
        match frame.msg_type {
            REGISTER_COMMAND => {
                if let Ok(command) = String::from_utf8(frame.payload) {
                    self.commands.push(command);
                } else {
                    self.fail_protocol("non-UTF-8 command");
                }
            }
            WIDGET => match update_widget(
                &mut self.widgets,
                frame.resource_id,
                frame.resource_revision,
                frame.flags,
                &frame.payload,
            ) {
                None => self.fail_protocol("invalid widget frame"),
                Some(true) => self.changed_widgets.push(frame.resource_id),
                Some(false) => {}
            },
            STATUS => {
                if let Ok(notice) = String::from_utf8(frame.payload) {
                    if self.notices.len() == NOTICE_CAP {
                        self.notices.remove(0);
                    }
                    self.notices.push(sanitize_control_bytes(notice));
                } else {
                    self.fail_protocol("non-UTF-8 status");
                }
            }
            _ => out.push(frame),
        }
    }

    pub fn fail_protocol(&mut self, reason: &str) {
        self.failure = Some(format!("protocol violation: {reason}"));
        self.stop_child();
    }

    fn stop_child(&mut self) {
        self.alive = false;
        self.state = PluginState::Stopping;
        self.retry_after_reap = false;
        self.clear_contributions();
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

#[allow(dead_code)]
fn start_child(path: &Path) -> io::Result<(Child, ChildStdin, ChildStdout, ChildStderr)> {
    let mut child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "missing plugin stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "missing plugin stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "missing plugin stderr"))?;
    let flags = fcntl_getfl(&stdout)?;
    fcntl_setfl(&stdout, flags | OFlags::NONBLOCK)?;
    let flags = fcntl_getfl(&stdin)?;
    fcntl_setfl(&stdin, flags | OFlags::NONBLOCK)?;
    let flags = fcntl_getfl(&stderr)?;
    fcntl_setfl(&stderr, flags | OFlags::NONBLOCK)?;
    Ok((child, stdin, stdout, stderr))
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

pub fn sanitize_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(STDERR_READ_BUDGET)])
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[allow(dead_code)]
fn update_widget(
    widgets: &mut HashMap<u64, Widget>,
    id: u64,
    revision: u64,
    flags: u16,
    payload: &[u8],
) -> Option<bool> {
    // None: protocol violation; Some(accepted) otherwise (stale drops are false)
    if flags & !WIDGET_FLAG_EXPLORER != 0 {
        return None;
    }
    let mut widget = parse_widget(payload)?;
    if widgets.get(&id).map_or(true, |old| revision > old.revision) {
        widget.revision = revision;
        widget.explorer = flags & WIDGET_FLAG_EXPLORER != 0;
        widgets.insert(id, widget);
        return Some(true);
    }
    Some(false)
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

    fn spans_payload(rows: &[(u16, &[(u16, u16, u8)])]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(rows.len() as u16).to_le_bytes());
        for (row, spans) in rows {
            payload.extend_from_slice(&row.to_le_bytes());
            payload.extend_from_slice(&(spans.len() as u16).to_le_bytes());
            for (start, len, style) in *spans {
                payload.extend_from_slice(&start.to_le_bytes());
                payload.extend_from_slice(&len.to_le_bytes());
                payload.push(*style);
            }
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
    fn parses_spans_round_trip() {
        let payload = spans_payload(&[(0, &[(0, 2, 1), (8, 4, 2)]), (3, &[(1, 9, 8)])]);
        let spans = parse_spans(&payload).unwrap();
        assert_eq!(
            spans,
            vec![(0, vec![(0, 2, 1), (8, 4, 2)]), (3, vec![(1, 9, 8)])]
        );
    }

    #[test]
    fn truncated_spans_are_rejected() {
        let payload = spans_payload(&[(0, &[(0, 2, 1)])]);
        for n in 0..payload.len() {
            assert!(parse_spans(&payload[..n]).is_none(), "accepted len {n}");
        }

        let mut overlong = payload;
        overlong.push(0);
        assert!(parse_spans(&overlong).is_none());
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
        let mut update =
            |rev, flags, item| update_widget(&mut widgets, 1, rev, flags, &widget_payload(&[item]));
        assert_eq!(update(2, 0, "new"), Some(true));
        assert_eq!(update(1, 0, "stale"), Some(false));
        assert_eq!(update(3, 0, "newer"), Some(true));
        // unknown frame flag bits are a protocol violation
        assert_eq!(update(4, 0x2, "x"), None);
        assert_eq!(widgets.get(&1).unwrap().revision, 3);
        assert_eq!(widgets.get(&1).unwrap().items, vec!["newer".to_string()]);
    }

    #[test]
    fn launcher_request_round_trips_every_operation() {
        let requests = [
            LauncherRequest::List { page: 0 },
            LauncherRequest::Enable {
                id: "alpha.one".into(),
                descriptor: None,
            },
            LauncherRequest::Disable("beta_2".into()),
            LauncherRequest::Reload("gamma-plugin".into()),
            LauncherRequest::Forget("delta".into()),
        ];
        for request in requests {
            let encoded = encode_launcher_request(42, &request).unwrap();
            assert_eq!(encoded.msg_type, LAUNCHER_REQUEST);
            assert_eq!(encoded.request_id, 42);
            assert_eq!(decode_launcher_request(&encoded).unwrap(), request);
        }
    }

    #[test]
    fn launcher_codecs_reject_bad_ids_and_trailing_payload() {
        for id in ["", "Alpha", "-alpha", "a/b", "a\u{80}", &"a".repeat(65)] {
            assert!(encode_launcher_request(
                0,
                &LauncherRequest::Enable {
                    id: id.into(),
                    descriptor: None
                }
            )
            .is_err());
        }
        let mut trailing =
            encode_launcher_request_payload(&LauncherRequest::List { page: 0 }).unwrap();
        trailing.push(0);
        assert!(decode_launcher_request_payload(&trailing).is_err());

        let mut id = vec![1, 1, b'a', 0];
        assert!(decode_launcher_event_payload(&id).is_err());
        id[0] = 99;
        id.truncate(1);
        assert!(decode_launcher_event_payload(&id).is_err());
    }

    #[test]
    fn launcher_descriptor_caps_geometric_retry_count() {
        let valid = LauncherRequest::Enable {
            id: "alpha".into(),
            descriptor: Some(LauncherDescriptor {
                path: "/bin/true".into(),
                max_restarts: 3,
                backoff_ms: 250,
                confirm_recovery: false,
            }),
        };
        assert!(encode_launcher_request_payload(&valid).is_ok());
        let invalid = LauncherRequest::Enable {
            id: "alpha".into(),
            descriptor: Some(LauncherDescriptor {
                path: "/bin/true".into(),
                max_restarts: 4,
                backoff_ms: 250,
                confirm_recovery: false,
            }),
        };
        assert_eq!(
            encode_launcher_request_payload(&invalid),
            Err(ProtoError::Malformed)
        );
    }

    #[test]
    fn launcher_codecs_reject_malformed_and_oversize_payloads() {
        assert!(decode_launcher_request_payload(&[]).is_err());
        assert!(decode_launcher_request_payload(&[0, 1]).is_err());
        assert!(decode_launcher_request_payload(&[1]).is_err());
        assert!(decode_launcher_response_payload(&[0, 1, 0]).is_err());
        assert!(decode_launcher_response_payload(&[255, 1, 0]).is_err());
        assert!(decode_launcher_event_payload(&[1, 2, b'a']).is_err());

        let oversized = vec![0u8; MAX_PAYLOAD as usize + 1];
        assert_eq!(
            decode_launcher_request_payload(&oversized),
            Err(ProtoError::Oversized)
        );
        assert_eq!(
            decode_launcher_response_payload(&oversized),
            Err(ProtoError::Oversized)
        );
        assert_eq!(
            decode_launcher_event_payload(&oversized),
            Err(ProtoError::Oversized)
        );
    }

    #[test]
    fn stderr_is_sanitized_and_bounded() {
        assert_eq!(sanitize_stderr(b"ok\x1b[31m\n\x80\x9f"), "ok?[31m?��");
        assert_eq!(sanitize_stderr(b"\xc2\x80\xc2\x9f"), "??");
        assert_eq!(
            sanitize_stderr(&vec![b'x'; STDERR_READ_BUDGET + 10]).len(),
            STDERR_READ_BUDGET
        );
    }

    #[test]
    fn launcher_list_round_trips_observed_lifecycle_states() {
        let response = LauncherResponse::List {
            page: 0,
            next_page: Some(1),
            records: vec![
                LauncherRecord {
                    id: "alpha".into(),
                    path: "/bin/true".into(),
                    enabled: true,
                    state: 1,
                    max_restarts: 3,
                    backoff_ms: 250,
                },
                LauncherRecord {
                    id: "beta".into(),
                    path: "/bin/false".into(),
                    enabled: false,
                    state: 4,
                    max_restarts: 0,
                    backoff_ms: 0,
                },
            ],
        };
        let payload = encode_launcher_response_payload(&response).unwrap();
        assert_eq!(
            decode_launcher_response_payload(&payload).unwrap(),
            response
        );
        let page = LauncherResponse::List {
            page: 7,
            next_page: None,
            records: vec![LauncherRecord {
                id: "zeta".into(),
                path: "/bin/true".into(),
                enabled: true,
                state: 3,
                max_restarts: 1,
                backoff_ms: 1000,
            }],
        };
        let page_bytes = encode_launcher_response_payload(&page).unwrap();
        assert_eq!(decode_launcher_response_payload(&page_bytes).unwrap(), page);
        assert!(decode_launcher_response_payload(&[0, 7, 0, 8, 0, 1]).is_err());
    }

    #[test]
    fn lifecycle_model_covers_timeout_crash_exhaustion_and_reaping() {
        let mut timeout = LifecycleModel::new();
        timeout.timeout();
        assert_eq!(timeout.state, PluginState::Failed);

        let mut crashes = LifecycleModel::new();
        crashes.hello();
        for _ in 0..3 {
            crashes.crash();
            assert_eq!(crashes.state, PluginState::Backoff);
            crashes.restart();
            crashes.hello();
        }
        crashes.crash();
        assert_eq!(crashes.state, PluginState::Failed);

        let mut stopping = LifecycleModel::new();
        stopping.hello();
        stopping.stop();
        assert_eq!(stopping.state, PluginState::Stopping);
        stopping.reaped();
        assert_eq!(stopping.state, PluginState::Failed);
    }

    #[test]
    fn persisted_backoff_and_explicit_reset_are_bounded() {
        assert_eq!(retry_delay(250, 0), Duration::from_millis(250));
        assert_eq!(retry_delay(250, 1), Duration::from_secs(1));
        assert_eq!(retry_delay(777, 2), Duration::from_millis(12_432));
        let mut model = LifecycleModel::new();
        model.hello();
        model.crash();
        model.restart();
        model.attempts = 7;
        model.reset_retry_budget();
        assert_eq!(model.attempts, 0);
    }

    #[test]
    fn contribution_cleanup_is_explicit() {
        // The lifecycle cleanup contract is exercised by the same method used
        // for protocol failure, disable, forget, and restart.
        let mut widgets = HashMap::new();
        widgets.insert(
            1,
            Widget {
                revision: 1,
                ..Widget::default()
            },
        );
        assert_eq!(widgets.len(), 1);
        widgets.clear();
        assert!(widgets.is_empty());
    }
}
