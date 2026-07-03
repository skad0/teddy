//! teddy — byte-addressed terminal editor. Single-threaded poll loop,
//! input first, paint on dirty (spec §18).

mod buffer;
#[allow(dead_code)] // wired into the S6 picker
mod ignore;
mod input;
#[allow(dead_code)] // wrapped into S8 plugin executables
mod lex;
mod render;
mod storage;
mod term;

use buffer::{Buffer, HUGE_WINDOW};
use input::{Key, Parser};
use render::{FrameBuf, TabInfo, View};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

struct Args {
    #[allow(dead_code)] // workspace root drives the S6 picker
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

    // open buffers before raw mode so errors print on a sane terminal
    let mut buffers: Vec<Buffer> = Vec::new();
    if args.files.is_empty() {
        buffers.push(Buffer::untitled());
    } else {
        for f in &args.files {
            match Buffer::open(f) {
                Ok(b) => buffers.push(b),
                Err(e) => {
                    eprintln!("teddy: {}: {e}", f.display());
                    return ExitCode::FAILURE;
                }
            }
        }
    }

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

    let result = run(&mut buffers);
    drop(guard);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("teddy: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Cap on bytes read per rendered row / prefix scan.
/// ponytail: degenerate multi-MB single lines render/measure only their
/// head; a windowed measure replaces this if it ever matters.
const LINE_CAP: usize = 256 * 1024;

fn run(buffers: &mut [Buffer]) -> std::io::Result<()> {
    let active = 0usize;
    let (mut cols, mut rows) = term::size();
    let mut dirty = true;

    let mut frame = FrameBuf::new();
    let mut parser = Parser::new();
    let mut keys: Vec<Key> = Vec::with_capacity(16);
    let mut read_buf = [0u8; 1024];
    let mut row_store: Vec<Vec<u8>> = Vec::new();
    let mut status_left = String::new();
    let mut status_right = String::new();
    let mut scratch = Vec::new();
    let mut out = std::io::stdout().lock();

    loop {
        // spec §18: input drains before paint (zero timeout while dirty)
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
            // ponytail: poll-tick resize check; SIGWINCH plumbing is S5
            let s = term::size();
            if s != (cols, rows) {
                (cols, rows) = s;
                dirty = true;
            }
        }

        let editor_rows = rows.saturating_sub(2).max(1) as u64;
        let buf = &mut buffers[active];
        for k in keys.drain(..) {
            match k {
                Key::Ctrl(b'Q') => return Ok(()),
                _ => {
                    if apply_movement(buf, k, editor_rows, &mut scratch) {
                        dirty = true;
                    }
                }
            }
        }

        if dirty && !term::poll_stdin(0)? {
            let buf = &mut buffers[active];
            let cursor_screen =
                build_view(buf, editor_rows as usize, cols as usize, &mut row_store, &mut scratch);
            format_status(buf, &mut status_left, &mut status_right);
            let tabs: Vec<TabInfo> = buffers
                .iter()
                .map(|b| TabInfo { name: &b.name, modified: b.modified })
                .collect(); // ponytail: tiny per-paint alloc, folded into S4 render cache
            let v = View {
                cols,
                rows,
                tabs: &tabs,
                active_tab: active,
                row_bytes: &row_store,
                left_col: buffers[active].left_col,
                cursor_screen,
                status_left: &status_left,
                status_right: &status_right,
            };
            render::paint(&mut frame, &v);
            out.write_all(frame.as_bytes())?;
            out.flush()?;
            dirty = false;
        }
    }
}

// --------------------------------------------------------------- movement

fn line_slice(buf: &mut Buffer, line: u64, out: &mut Vec<u8>) {
    let s = buf.line_start(line);
    let e = buf.line_end(line);
    out.clear();
    buf.read_range(s, (e - s).min(LINE_CAP as u64), out);
}

/// Returns true if anything changed (cursor or viewport).
fn apply_movement(buf: &mut Buffer, key: Key, editor_rows: u64, scratch: &mut Vec<u8>) -> bool {
    let before = (buf.cursor, buf.top_line, buf.top_byte);
    if buf.huge || buf.line_index.is_none() {
        huge_movement(buf, key, editor_rows, scratch);
    } else {
        normal_movement(buf, key, editor_rows, scratch);
    }
    (buf.cursor, buf.top_line, buf.top_byte) != before
}

fn normal_movement(buf: &mut Buffer, key: Key, editor_rows: u64, scratch: &mut Vec<u8>) {
    let line = buf.line_of_byte(buf.cursor);
    let update_goal = |buf: &mut Buffer, scratch: &mut Vec<u8>| {
        let l = buf.line_of_byte(buf.cursor);
        let start = buf.line_start(l);
        line_slice(buf, l, scratch);
        buf.goal_col = render::visual_col(scratch, (buf.cursor - start) as usize);
    };
    let vertical = |buf: &mut Buffer, target: u64, scratch: &mut Vec<u8>| {
        line_slice(buf, target, scratch);
        buf.cursor = buf.line_start(target) + render::byte_at_col(scratch, buf.goal_col) as u64;
    };
    match key {
        Key::Left => {
            buf.cursor = buf.prev_boundary(buf.cursor);
            update_goal(buf, scratch);
        }
        Key::Right => {
            buf.cursor = buf.next_boundary(buf.cursor);
            update_goal(buf, scratch);
        }
        Key::Up if line > 0 => vertical(buf, line - 1, scratch),
        Key::Down if line + 1 < buf.line_count() => vertical(buf, line + 1, scratch),
        Key::PageUp => vertical(buf, line.saturating_sub(editor_rows), scratch),
        Key::PageDown => vertical(buf, (line + editor_rows).min(buf.line_count() - 1), scratch),
        Key::Home => {
            buf.cursor = buf.line_start(line);
            buf.goal_col = 0;
        }
        Key::End => {
            buf.cursor = buf.line_end(line);
            update_goal(buf, scratch);
        }
        _ => {}
    }
}

/// Huge/unindexed movement: rows are reconstructed from a byte window
/// around top_byte; navigation is line-start hopping within that window.
fn huge_movement(buf: &mut Buffer, key: Key, editor_rows: u64, scratch: &mut Vec<u8>) {
    let starts = window_row_starts(buf, editor_rows as usize + 1, scratch);
    let row_of = |c: u64| starts.iter().rposition(|&s| s <= c).unwrap_or(0);
    match key {
        Key::Left => buf.cursor = buf.prev_boundary(buf.cursor).max(0),
        Key::Right => {
            let n = buf.next_boundary(buf.cursor);
            buf.cursor = n.min(buf.len());
        }
        Key::Down => {
            let r = row_of(buf.cursor);
            if r + 1 < starts.len() {
                buf.cursor = starts[r + 1];
                if r + 2 >= starts.len() && starts.len() > 1 {
                    buf.top_byte = starts[1]; // scroll one row
                }
            }
        }
        Key::Up => {
            let r = row_of(buf.cursor);
            if r > 0 {
                buf.cursor = starts[r - 1];
            } else if buf.top_byte > 0 {
                buf.top_byte = prev_line_start(buf, buf.top_byte, scratch);
                buf.cursor = buf.top_byte;
            }
        }
        Key::PageDown => {
            if let Some(&last) = starts.last() {
                buf.top_byte = last;
                buf.cursor = last;
            }
        }
        Key::PageUp => {
            for _ in 0..editor_rows {
                if buf.top_byte == 0 {
                    break;
                }
                buf.top_byte = prev_line_start(buf, buf.top_byte, scratch);
            }
            buf.cursor = buf.top_byte;
        }
        Key::Home => buf.cursor = starts[row_of(buf.cursor)],
        Key::End => {
            let r = row_of(buf.cursor);
            let end = starts.get(r + 1).map(|s| s - 1).unwrap_or(buf.len());
            buf.cursor = end;
        }
        _ => {}
    }
    if buf.cursor < buf.top_byte {
        buf.top_byte = buf.cursor;
    }
}

/// Starts of up to `n` display rows beginning at top_byte.
fn window_row_starts(buf: &mut Buffer, n: usize, scratch: &mut Vec<u8>) -> Vec<u64> {
    scratch.clear();
    let take = HUGE_WINDOW.min(buf.len().saturating_sub(buf.top_byte));
    buf.read_range(buf.top_byte, take, scratch);
    let mut starts = vec![buf.top_byte];
    for (i, &b) in scratch.iter().enumerate() {
        if starts.len() >= n {
            break;
        }
        if b == b'\n' {
            starts.push(buf.top_byte + i as u64 + 1);
        }
    }
    starts
}

fn prev_line_start(buf: &mut Buffer, from: u64, scratch: &mut Vec<u8>) -> u64 {
    if from == 0 {
        return 0;
    }
    let back = 4096.min(from);
    scratch.clear();
    buf.read_range(from - back, back, scratch);
    // skip the newline that terminates the previous line
    let upto = scratch.len().saturating_sub(1);
    match scratch[..upto].iter().rposition(|&b| b == b'\n') {
        Some(i) => from - back + i as u64 + 1,
        None if back < from => from - back, // ponytail: mid-line landing on very long lines
        None => 0,
    }
}

// ----------------------------------------------------------------- view

/// Scroll to keep the cursor visible, fill row_store, return screen cursor.
fn build_view(
    buf: &mut Buffer,
    editor_rows: usize,
    cols: usize,
    row_store: &mut Vec<Vec<u8>>,
    scratch: &mut Vec<u8>,
) -> (u16, u16) {
    row_store.iter_mut().for_each(|r| r.clear());
    while row_store.len() < editor_rows {
        row_store.push(Vec::new());
    }
    row_store.truncate(editor_rows);

    if buf.huge || buf.line_index.is_none() {
        let mut starts = window_row_starts(buf, editor_rows + 1, scratch);
        let mut r = starts.iter().rposition(|&s| s <= buf.cursor).unwrap_or(0);
        // cursor left the visible rows (below): rebase window on its line
        if r >= editor_rows || buf.cursor > buf.top_byte + HUGE_WINDOW {
            buf.top_byte = prev_line_start(buf, (buf.cursor + 1).min(buf.len()), scratch);
            starts = window_row_starts(buf, editor_rows + 1, scratch);
            r = starts.iter().rposition(|&s| s <= buf.cursor).unwrap_or(0);
        }
        let r = r.min(editor_rows - 1);
        for (i, slot) in row_store.iter_mut().enumerate() {
            if let Some(&s) = starts.get(i) {
                let end = starts.get(i + 1).map(|e| e - 1).unwrap_or_else(|| {
                    (s + LINE_CAP as u64).min(buf.len())
                });
                buf.read_range(s, (end - s).min(LINE_CAP as u64), slot);
            }
        }
        let row_start = starts.get(r).copied().unwrap_or(buf.top_byte);
        scratch.clear();
        buf.read_range(row_start, (buf.cursor - row_start).min(LINE_CAP as u64), scratch);
        let vcol = render::visual_col(scratch, scratch.len());
        clamp_left(buf, vcol, cols);
        return ((vcol - buf.left_col) as u16, r as u16);
    }

    let cursor_line = buf.line_of_byte(buf.cursor);
    if cursor_line < buf.top_line {
        buf.top_line = cursor_line;
    }
    if cursor_line >= buf.top_line + editor_rows as u64 {
        buf.top_line = cursor_line - editor_rows as u64 + 1;
    }
    for (i, slot) in row_store.iter_mut().enumerate() {
        let line = buf.top_line + i as u64;
        if line < buf.line_count() {
            let s = buf.line_start(line);
            let e = buf.line_end(line);
            buf.read_range(s, (e - s).min(LINE_CAP as u64), slot);
        }
    }
    let line_start = buf.line_start(cursor_line);
    scratch.clear();
    buf.read_range(line_start, (buf.cursor - line_start).min(LINE_CAP as u64), scratch);
    let vcol = render::visual_col(scratch, scratch.len());
    clamp_left(buf, vcol, cols);
    ((vcol - buf.left_col) as u16, (cursor_line - buf.top_line) as u16)
}

fn clamp_left(buf: &mut Buffer, vcol: usize, cols: usize) {
    if vcol < buf.left_col {
        buf.left_col = vcol;
    }
    if vcol >= buf.left_col + cols {
        buf.left_col = vcol - cols + 1;
    }
}

fn format_status(buf: &mut Buffer, left: &mut String, right: &mut String) {
    left.clear();
    right.clear();
    let _ = write!(left, " {}", buf.name);
    if buf.modified {
        left.push('*');
    }
    if buf.readonly {
        left.push_str("  RO");
    }
    if buf.binary {
        left.push_str("  BIN");
    }
    if buf.huge {
        left.push_str("  HUGE");
    }
    if buf.line_index.is_none() {
        left.push_str("  NOIDX");
    }
    if buf.io_error {
        left.push_str("  IOERR");
    }
    if buf.huge || buf.line_index.is_none() {
        let pct = if buf.len() == 0 { 100 } else { buf.cursor * 100 / buf.len() };
        let _ = write!(right, "byte {} / {} ({}%)  Ctrl+Q quit ", buf.cursor, buf.len(), pct);
    } else {
        let line = buf.line_of_byte(buf.cursor);
        let _ = write!(right, "Ln {}, Col {}  Ctrl+Q quit ", line + 1, buf.goal_col + 1);
    }
}
