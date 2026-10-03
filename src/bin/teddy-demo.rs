#[path = "../plugin.rs"]
mod plugin;

use plugin::{
    encode, encode_widget, Frame, FrameReader, Widget, COMMAND_INVOKE, EDIT_RESULT, EDIT_TX, HELLO,
    IN_COLLAPSE, IN_EXPAND, PROTO_VERSION, REGISTER_COMMAND, STATUS, TREE_EXPANDED,
    TREE_HAS_CHILDREN, WIDGET, WIDGET_EVENT, WIDGET_FLAG_EXPLORER, WIDGET_INPUT, W_TREE,
};
use std::io::{self, Read, Write};

fn main() -> io::Result<()> {
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 8192];
    let mut remembered_buffer = 0;
    let mut remembered_revision = 0;
    // Ctrl+T explorer: fixed two-folder tree; the plugin owns expansion
    let mut expanded = [false; 2];
    let mut explorer_revision = 1u64;
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
                        write_frame(&mut stdout, &explorer_frame(&expanded, explorer_revision))?;
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
                WIDGET_INPUT
                    if frame.resource_id == EXPLORER
                        && matches!(frame.payload.first(), Some(&(IN_EXPAND | IN_COLLAPSE))) =>
                {
                    let index = frame
                        .payload
                        .get(1..5)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                    let row = index.and_then(|i| explorer_rows(&expanded).get(i as usize).copied());
                    if frame.resource_revision == explorer_revision {
                        if let Some((Some(folder), _, _)) = row {
                            expanded[folder] = frame.payload[0] == IN_EXPAND;
                            explorer_revision += 1;
                            write_frame(
                                &mut stdout,
                                &explorer_frame(&expanded, explorer_revision),
                            )?;
                        }
                    }
                }
                WIDGET_EVENT if frame.resource_id == EXPLORER => {
                    let index = frame
                        .payload
                        .get(0..4)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                    if let Some((_, _, name)) =
                        index.and_then(|i| explorer_rows(&expanded).get(i as usize).copied())
                    {
                        write_frame(
                            &mut stdout,
                            &Frame {
                                msg_type: STATUS,
                                flags: 0,
                                request_id: 0,
                                resource_id: 0,
                                resource_revision: 0,
                                payload: format!("demo: picked {name}").into_bytes(),
                            },
                        )?;
                    }
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

const EXPLORER: u64 = 2;

/// Visible explorer rows: (folder index if a folder, depth, name).
fn explorer_rows(expanded: &[bool; 2]) -> Vec<(Option<usize>, u8, &'static str)> {
    let tree: [(&str, &[&str]); 2] = [("src", &["main.rs", "pane.rs"]), ("docs", &["plan.md"])];
    let mut rows = Vec::new();
    for (i, (folder, files)) in tree.iter().enumerate() {
        rows.push((Some(i), 0, *folder));
        if expanded[i] {
            rows.extend(files.iter().map(|f| (None, 1, *f)));
        }
    }
    rows
}

fn explorer_frame(expanded: &[bool; 2], revision: u64) -> Frame {
    let rows = explorer_rows(expanded);
    let widget = Widget {
        kind: W_TREE,
        items: rows.iter().map(|r| r.2.to_string()).collect(),
        tree: rows
            .iter()
            .map(|&(folder, depth, _)| match folder {
                Some(i) if expanded[i] => (depth, TREE_HAS_CHILDREN | TREE_EXPANDED),
                Some(_) => (depth, TREE_HAS_CHILDREN),
                None => (depth, 0),
            })
            .collect(),
        ..Widget::default()
    };
    Frame {
        msg_type: WIDGET,
        flags: WIDGET_FLAG_EXPLORER,
        request_id: 0,
        resource_id: EXPLORER,
        resource_revision: revision,
        payload: encode_widget(&widget),
    }
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
