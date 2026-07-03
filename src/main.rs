//! teddy — Stage 0 core skeleton. Single-threaded poll loop, input first,
//! paint on dirty (spec §18).

mod input;
mod render;
mod term;

use input::{Key, Parser};
use render::{FrameBuf, View};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

struct Args {
    workspace: Option<PathBuf>,
    files: Vec<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { workspace: None, files: Vec::new() };
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("-w") => {
                let root = it.next().ok_or("-w requires a directory argument")?;
                args.workspace = Some(PathBuf::from(root));
            }
            Some("-h") | Some("--help") => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            Some("-V") | Some("--version") => {
                println!("teddy {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => args.files.push(PathBuf::from(a)),
        }
    }
    // spec §17: directories as positional args are rejected unless -w
    for f in &args.files {
        if f.is_dir() {
            return Err(format!("{}: is a directory (use -w to open a workspace)", f.display()));
        }
    }
    Ok(args)
}

const USAGE: &str = "usage: teddy [-w workspace-root] [file ...]";

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    // panic path per spec §20: restore terminal before the default report
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term::restore();
        default_hook(info);
    }));

    let guard = match term::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("teddy: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = run(&args);
    drop(guard);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("teddy: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> std::io::Result<()> {
    let tabs: Vec<String> = if args.files.is_empty() {
        vec!["untitled".to_string()]
    } else {
        args.files
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string())
            })
            .collect()
    };
    let active_tab = 0usize;

    let (mut cols, mut rows) = term::size();
    // ponytail: cursor roams the empty viewport; Stage 1 clamps it to buffer content
    let mut cursor: (u16, u16) = (0, 0);
    let mut dirty = true;

    let mut frame = FrameBuf::new();
    let mut parser = Parser::new();
    let mut keys: Vec<Key> = Vec::with_capacity(16);
    let mut read_buf = [0u8; 1024];
    let mut out = std::io::stdout().lock();

    loop {
        // spec §18: input drains before paint. Dirty state polls with zero
        // timeout so already-buffered input is applied before the frame is
        // built; 10ms while an escape sequence is pending so a lone ESC
        // resolves fast; otherwise the idle tick doubles as the resize check.
        let timeout = if dirty { 0 } else if parser.has_pending() { 10 } else { 250 };
        if term::poll_stdin(timeout)? {
            let n = term::read_stdin(&mut read_buf)?;
            if n == 0 {
                return Ok(()); // EOF: controlling terminal went away
            }
            parser.feed(&read_buf[..n], &mut keys);
        } else {
            if timeout > 0 {
                parser.flush_timeout(&mut keys);
            }
            // ponytail: poll-tick resize check instead of a SIGWINCH handler;
            // signal plumbing lands with Stage 5 watcher/signal work
            let s = term::size();
            if s != (cols, rows) {
                (cols, rows) = s;
                cursor.0 = cursor.0.min(cols.saturating_sub(1));
                cursor.1 = cursor.1.min(rows.saturating_sub(2).max(1) - 1);
                dirty = true;
            }
        }

        let editor_rows = rows.saturating_sub(2).max(1);
        for k in keys.drain(..) {
            let before = cursor;
            match k {
                Key::Ctrl(b'Q') => return Ok(()),
                Key::Up => cursor.1 = cursor.1.saturating_sub(1),
                Key::Down => cursor.1 = (cursor.1 + 1).min(editor_rows - 1),
                Key::Left => cursor.0 = cursor.0.saturating_sub(1),
                Key::Right => cursor.0 = (cursor.0 + 1).min(cols.saturating_sub(1)),
                Key::Home => cursor.0 = 0,
                Key::End => cursor.0 = cols.saturating_sub(1),
                Key::PageUp => cursor.1 = 0,
                Key::PageDown => cursor.1 = editor_rows - 1,
                _ => {} // editing keys land in Stage 2, palette/search in S6/S3
            }
            if cursor != before {
                dirty = true;
            }
        }

        // paint only when no further input is queued (input priority)
        if dirty && !term::poll_stdin(0)? {
            let v = View { cols, rows, tabs: &tabs, active_tab, cursor };
            render::paint(&mut frame, &v);
            out.write_all(frame.as_bytes())?;
            out.flush()?;
            dirty = false;
        }
    }
}
