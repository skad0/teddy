#[path = "../plugin.rs"]
mod plugin;

use plugin::{
    encode, Frame, FrameReader, COMMAND_INVOKE, HELLO, PROTO_VERSION, REGISTER_COMMAND, STATUS,
};
use std::io::{self, Read, Write};

fn main() -> io::Result<()> {
    shell("ai", "ai shell: no provider configured")
}

fn shell(command: &str, message: &str) -> io::Result<()> {
    let mut reader = FrameReader::new();
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 8192];

    loop {
        match stdin.read(&mut buf)? {
            0 => return Ok(()),
            n => reader.push(&buf[..n]),
        }
        while let Some(frame) = reader.next().map_err(proto_io)? {
            match frame.msg_type {
                HELLO
                    if frame.payload.len() == 4
                        && u32::from_le_bytes(frame.payload[0..4].try_into().unwrap())
                            == PROTO_VERSION =>
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
                            payload: command.as_bytes().to_vec(),
                        },
                    )?;
                }
                COMMAND_INVOKE => {
                    // ponytail: shells prove the lifecycle; real behavior is out of scope for core.
                    write_frame(
                        &mut stdout,
                        &Frame {
                            msg_type: STATUS,
                            flags: 0,
                            request_id: frame.request_id,
                            resource_id: frame.resource_id,
                            resource_revision: frame.resource_revision,
                            payload: message.as_bytes().to_vec(),
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

fn proto_io(e: plugin::ProtoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}"))
}
