#[path = "../plugin.rs"]
mod plugin;

use plugin::{
    encode, Frame, FrameReader, COMMAND_INVOKE, EDIT_RESULT, EDIT_TX, HELLO, PROTO_VERSION,
    REGISTER_COMMAND, STATUS, WIDGET, WIDGET_EVENT,
};
use std::io::{self, Read, Write};

fn main() -> io::Result<()> {
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 8192];
    let mut remembered_buffer = 0;
    let mut remembered_revision = 0;
    let mut stdout = io::stdout().lock();
    let mut stdin = io::stdin().lock();

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
                                flags: 0,
                                request_id: 0,
                                resource_id: 0,
                                resource_revision: 0,
                                payload: PROTO_VERSION.to_le_bytes().to_vec(),
                            },
                        )?;
                        write_frame(
                            &mut stdout,
                            &Frame {
                                msg_type: REGISTER_COMMAND,
                                flags: 0,
                                request_id: 0,
                                resource_id: 0,
                                resource_revision: 0,
                                payload: b"demo".to_vec(),
                            },
                        )?;
                    }
                }
                COMMAND_INVOKE => {
                    remembered_buffer = frame.resource_id;
                    remembered_revision = frame.resource_revision;
                    write_frame(
                        &mut stdout,
                        &Frame {
                            msg_type: WIDGET,
                            flags: 0,
                            request_id: 0,
                            resource_id: 1,
                            resource_revision: 1,
                            payload: widget_payload(&["insert marker at start", "do nothing"]),
                        },
                    )?;
                }
                WIDGET_EVENT => {
                    if frame.payload.len() == 4
                        && u32::from_le_bytes(frame.payload[0..4].try_into().unwrap()) == 0
                    {
                        let mut payload = Vec::new();
                        payload.extend_from_slice(&0u64.to_le_bytes());
                        payload.extend_from_slice(&0u64.to_le_bytes());
                        payload.extend_from_slice(b"// teddy demo\n");
                        write_frame(
                            &mut stdout,
                            &Frame {
                                msg_type: EDIT_TX,
                                flags: 0,
                                request_id: 1,
                                resource_id: remembered_buffer,
                                resource_revision: remembered_revision,
                                payload,
                            },
                        )?;
                    }
                }
                EDIT_RESULT => {
                    let ok = frame.payload.first().copied() == Some(1);
                    let notice = if ok {
                        "demo: edit ok"
                    } else {
                        "demo: edit rejected"
                    };
                    write_frame(
                        &mut stdout,
                        &Frame {
                            msg_type: STATUS,
                            flags: 0,
                            request_id: 0,
                            resource_id: 0,
                            resource_revision: 0,
                            payload: notice.as_bytes().to_vec(),
                        },
                    )?;
                }
                _ => {}
            }
        }
    }
}

fn write_frame<W: Write>(out: &mut W, frame: &Frame) -> io::Result<()> {
    let mut bytes = Vec::new();
    encode(frame, &mut bytes);
    out.write_all(&bytes)?;
    out.flush()
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

fn proto_io(e: plugin::ProtoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trip() {
        let original = Frame {
            msg_type: COMMAND_INVOKE,
            flags: 1,
            request_id: 2,
            resource_id: 3,
            resource_revision: 4,
            payload: b"args".to_vec(),
        };
        let mut bytes = Vec::new();
        encode(&original, &mut bytes);

        let mut reader = FrameReader::new();
        reader.push(&bytes[..10]);
        assert!(reader.next().unwrap().is_none());
        reader.push(&bytes[10..]);
        let got = reader.next().unwrap().unwrap();

        assert_eq!(got.msg_type, original.msg_type);
        assert_eq!(got.flags, original.flags);
        assert_eq!(got.request_id, original.request_id);
        assert_eq!(got.resource_id, original.resource_id);
        assert_eq!(got.resource_revision, original.resource_revision);
        assert_eq!(got.payload, original.payload);
    }
}
