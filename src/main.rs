//! teddy — byte-addressed terminal editor. Single-threaded poll loop,
//! input first, paint on dirty (spec §18).

mod buffer;
#[allow(dead_code)] // wired into the S6 picker
mod ignore;
mod input;
#[allow(dead_code)] // wrapped into S8 plugin executables
mod lex;
mod lines;
mod render;
mod search;
mod storage;
mod term;
mod watch;

use buffer::{Buffer, HUGE_WINDOW};
use input::{Key, Parser};
use render::{FrameBuf, RenderCache, View};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

struct Args {
    #[allow(dead_code)] // workspace root drives the S6 picker
    workspace: Option<PathBuf>,
    files: Vec<PathBuf>,
    follow: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { workspace: None, files: Vec::new(), follow: false };
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("-w") => {
                let root = it.next().ok_or("-w requires a directory argument")?;
                args.workspace = Some(PathBuf::from(root));
            }
            Some("-F") | Some("--follow") => args.follow = true,
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

const USAGE: &str = "usage: teddy [-w workspace-root] [-F|--follow] [file ...]";

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
                Ok(mut b) => {
                    if args.follow {
                        b.follow = true;
                        b.readonly = true; // reload discards edits anyway
                        b.cursor = b.len();
                    }
                    buffers.push(b)
                }
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

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Edit,
    ConfirmQuit,
    /// Statusline text prompt (find / replace flows).
    Prompt(PromptKind),
    /// Interactive replace: a match is highlighted, or the search job is
    /// still hunting for the next one.
    ReplaceConfirm,
}

#[derive(PartialEq, Clone, Copy)]
enum PromptKind {
    Find,
    ReplaceNeedle,
    ReplaceWith,
}

/// Bytes scanned per idle work slice (spec §18 fixed slices).
const SLICE: u64 = 2 * 1024 * 1024;

// undo grouping kinds
const KIND_NONE: u8 = 0;
const KIND_INSERT: u8 = 1;
const KIND_BACKSPACE: u8 = 2;
const KIND_DELETE: u8 = 3;

fn run(buffers: &mut [Buffer]) -> std::io::Result<()> {
    let active = 0usize;
    let (mut cols, mut rows) = term::size();
    let mut dirty = true;
    let mut mode = Mode::Edit;
    let mut status_msg = String::new();
    let mut pending_force_save = false;
    let mut last_edit_kind = KIND_NONE;
    // find/replace state
    let mut prompt = String::new();
    let mut find_needle: Vec<u8> = Vec::new();
    let mut replace_with: Vec<u8> = Vec::new();
    let mut search_job: Option<search::Search> = None;
    let mut replace_job: Option<search::ReplaceAll> = None;
    let mut confirm_match: Option<u64> = None;
    let mut replace_scope_end: u64 = 0;
    let mut replace_count: u64 = 0;
    let mut job_scratch: Vec<u8> = Vec::new();

    let mut frame = FrameBuf::new();
    let mut render_cache = RenderCache::new();
    let mut parser = Parser::new();
    let mut keys: Vec<Key> = Vec::with_capacity(16);
    let mut read_buf = [0u8; 1024];
    let mut row_store: Vec<Vec<u8>> = Vec::new();
    let mut row_sel: Vec<Option<(usize, usize)>> = Vec::new();
    let mut starts_scratch: Vec<u64> = Vec::new();
    let mut status_left = String::new();
    let mut status_right = String::new();
    let mut scratch = Vec::new();
    // owned tab snapshot, rebuilt only when a name/modified flag changes
    let mut tabs_buf: Vec<(String, bool)> = Vec::new();
    let mut out = std::io::stdout().lock();
    #[cfg(feature = "perf")]
    let mut perf = perf::Perf::from_env();

    // spec §13: watch open files; a watcher that can't start degrades to
    // no external-change detection rather than failing the editor
    let mut watcher = watch::Watcher::new().ok();
    if let Some(w) = watcher.as_mut() {
        for (i, b) in buffers.iter().enumerate() {
            if let Some(p) = &b.path {
                let _ = w.watch(i as u64, p); // new-file buffers: not on disk yet
            }
        }
    }
    let mut watch_tokens: Vec<u64> = Vec::new();

    loop {
        // spec §18: input drains before paint (zero timeout while dirty
        // or while cooperative jobs want their next slice)
        let jobs_active = search_job.is_some()
            || replace_job.is_some()
            || buffers[active].index_build.is_some();
        let timeout =
            if dirty || jobs_active { 0 } else if parser.has_pending() { 10 } else { 250 };
        let (stdin_ready, watch_ready) =
            term::poll_stdin(watcher.as_ref().map(|w| w.fd()), timeout)?;
        if stdin_ready {
            let n = term::read_stdin(&mut read_buf)?;
            if n == 0 {
                return Ok(()); // EOF: controlling terminal went away
            }
            #[cfg(feature = "perf")]
            perf.frame_start();
            parser.feed(&read_buf[..n], &mut keys);
        } else {
            if timeout > 0 && !watch_ready {
                parser.flush_timeout(&mut keys);
            }
            // ponytail: poll-tick resize check; SIGWINCH would only save
            // one 250ms tick of latency
            let s = term::size();
            if s != (cols, rows) {
                (cols, rows) = s;
                dirty = true;
            }
        }

        if watch_ready {
            let w = watcher.as_mut().expect("watch_ready without watcher");
            watch_tokens.clear();
            w.drain(&mut watch_tokens);
            for &t in &watch_tokens {
                let Some(buf) = buffers.get_mut(t as usize) else { continue };
                // re-arm FIRST: an atomic-replace writer left a new inode
                // behind, and arming before the stat/reload means any write
                // that lands after this line fires a fresh event (no
                // missed-change window)
                if let Some(p) = buf.path.clone() {
                    let _ = w.watch(t, &p);
                }
                // stat confirmation (spec §13): our own saves and event
                // bursts that net out to the recorded state are ignored
                if !buf.disk_changed() {
                    continue;
                }
                if buf.follow || !buf.modified() {
                    // clean buffers and follow mode hard-reload
                    if buf.reload().is_ok() {
                        status_msg.clear();
                        let _ = write!(status_msg, "{}: reloaded (changed on disk)", buf.name);
                        if t as usize == active {
                            // in-flight jobs hold byte offsets into the old content
                            search_job = None;
                            replace_job = None;
                            confirm_match = None;
                            // prompt text survives a reload; a highlighted
                            // match does not
                            if mode == Mode::ReplaceConfirm {
                                mode = Mode::Edit;
                            }
                        }
                    }
                } else {
                    buf.external_change = true;
                    status_msg.clear();
                    let _ = write!(status_msg, "{}: file changed on disk", buf.name);
                }
                dirty = true;
            }
        }

        let editor_rows = rows.saturating_sub(2).max(1) as u64;
        for k in keys.drain(..) {
            dirty = true;
            status_msg.clear();
            match mode {
                Mode::ConfirmQuit => {
                    match k {
                        Key::Char('y') | Key::Char('Y') => return Ok(()),
                        Key::Char('s') | Key::Char('S') => {
                            let mut all_saved = true;
                            for b in buffers.iter_mut() {
                                if b.modified() {
                                    if let Err(e) = b.save(false) {
                                        let _ = write!(status_msg, "{}: {e}", b.name);
                                        all_saved = false;
                                        break;
                                    }
                                }
                            }
                            if all_saved {
                                return Ok(());
                            }
                            mode = Mode::Edit;
                        }
                        _ => mode = Mode::Edit,
                    }
                    continue;
                }
                Mode::Prompt(kind) => {
                    match k {
                        // 64 KiB cap keeps one search slice within budget
                        Key::Char(c) if prompt.len() < 64 * 1024 => prompt.push(c),
                        Key::Char(_) => {}
                        Key::Backspace => {
                            prompt.pop();
                        }
                        Key::Esc => {
                            prompt.clear();
                            mode = Mode::Edit;
                        }
                        Key::Enter => match kind {
                            PromptKind::Find => {
                                mode = Mode::Edit;
                                if !prompt.is_empty() {
                                    find_needle = prompt.as_bytes().to_vec();
                                    search_job = Some(search::Search::new(
                                        find_needle.clone(),
                                        buffers[active].cursor,
                                    ));
                                }
                                prompt.clear();
                            }
                            PromptKind::ReplaceNeedle => {
                                if prompt.is_empty() {
                                    mode = Mode::Edit;
                                } else {
                                    find_needle = prompt.as_bytes().to_vec();
                                    prompt.clear();
                                    mode = Mode::Prompt(PromptKind::ReplaceWith);
                                }
                            }
                            PromptKind::ReplaceWith => {
                                replace_with = prompt.as_bytes().to_vec();
                                prompt.clear();
                                let buf = &mut buffers[active];
                                let (from, end) =
                                    buf.selection().unwrap_or((buf.cursor, buf.len()));
                                replace_scope_end = end;
                                replace_count = 0;
                                buf.group_counter += 1;
                                buf.sel_anchor = None;
                                search_job =
                                    Some(search::Search::new_no_wrap(find_needle.clone(), from));
                                confirm_match = None;
                                mode = Mode::ReplaceConfirm;
                            }
                        },
                        _ => {}
                    }
                    continue;
                }
                Mode::ReplaceConfirm => {
                    match (k, confirm_match) {
                        (Key::Esc, _) => {
                            search_job = None;
                            replace_job = None;
                            confirm_match = None;
                            buffers[active].sel_anchor = None;
                            let _ = write!(status_msg, "replaced {replace_count}");
                            mode = Mode::Edit;
                        }
                        (Key::Char('y') | Key::Enter, Some(at)) => {
                            let buf = &mut buffers[active];
                            let n = find_needle.len() as u64;
                            let g = buf.group_counter;
                            match buf.replace(at, at + n, &replace_with, g) {
                                Ok(()) => {
                                    replace_count += 1;
                                    let delta = replace_with.len() as i64 - n as i64;
                                    replace_scope_end =
                                        (replace_scope_end as i64 + delta) as u64;
                                    buf.cursor = at + replace_with.len() as u64;
                                    buf.sel_anchor = None;
                                    search_job = Some(search::Search::new_no_wrap(
                                        find_needle.clone(),
                                        buf.cursor,
                                    ));
                                    confirm_match = None;
                                }
                                Err(e) => {
                                    status_msg.push_str(e);
                                    mode = Mode::Edit;
                                }
                            }
                        }
                        (Key::Char('n'), Some(at)) => {
                            search_job = Some(search::Search::new_no_wrap(
                                find_needle.clone(),
                                at + 1,
                            ));
                            confirm_match = None;
                            buffers[active].sel_anchor = None;
                        }
                        (Key::Char('a'), Some(at)) => {
                            let buf = &mut buffers[active];
                            if buf.huge {
                                // spec §10: replace-all disabled in huge mode
                                status_msg.push_str("replace-all is disabled for huge files");
                            } else {
                                let g = buf.group_counter;
                                buf.sel_anchor = None;
                                replace_job = Some(search::ReplaceAll::new(
                                    find_needle.clone(),
                                    replace_with.clone(),
                                    at,
                                    replace_scope_end,
                                    g,
                                ));
                                confirm_match = None;
                                search_job = None;
                            }
                        }
                        _ => {} // still hunting, or unbound key
                    }
                    continue;
                }
                Mode::Edit => {}
            }
            // any manual key cancels an in-flight search (spec: cancellable)
            if search_job.is_some() {
                search_job = None;
            }
            if !matches!(k, Key::Ctrl(b'S')) {
                pending_force_save = false;
            }
            match k {
                Key::Ctrl(b'F') => {
                    last_edit_kind = KIND_NONE;
                    prompt.clear();
                    mode = Mode::Prompt(PromptKind::Find);
                }
                Key::Ctrl(b'G') => {
                    last_edit_kind = KIND_NONE;
                    if find_needle.is_empty() {
                        status_msg.push_str("no previous search");
                    } else {
                        search_job = Some(search::Search::new(
                            find_needle.clone(),
                            buffers[active].cursor,
                        ));
                    }
                }
                Key::Ctrl(b'R') => {
                    last_edit_kind = KIND_NONE;
                    if buffers[active].readonly {
                        status_msg.push_str("buffer is read-only");
                    } else {
                        prompt.clear();
                        mode = Mode::Prompt(PromptKind::ReplaceNeedle);
                    }
                }
                Key::Ctrl(b'Q') => {
                    if buffers.iter().any(|b| b.modified()) {
                        mode = Mode::ConfirmQuit;
                    } else {
                        return Ok(());
                    }
                }
                Key::Ctrl(b'S') => {
                    last_edit_kind = KIND_NONE;
                    let buf = &mut buffers[active];
                    match buf.save(pending_force_save) {
                        Ok(()) => {
                            let _ = write!(status_msg, "saved {}", buf.name);
                            pending_force_save = false;
                            // our rename left a new inode: re-arm the watch
                            if let (Some(w), Some(p)) = (watcher.as_mut(), buf.path.clone()) {
                                let _ = w.watch(active as u64, &p);
                            }
                        }
                        Err(e) if e.to_string() == "file changed on disk" => {
                            status_msg.push_str("file changed on disk — Ctrl+S again to overwrite");
                            pending_force_save = true;
                        }
                        Err(e) => {
                            let _ = write!(status_msg, "save failed: {e}");
                            pending_force_save = false;
                        }
                    }
                }
                Key::Ctrl(b'Z') => {
                    last_edit_kind = KIND_NONE;
                    let buf = &mut buffers[active];
                    if !buf.undo_group() {
                        status_msg.push_str("nothing to undo");
                    }
                    buf.group_counter += 1;
                }
                Key::Ctrl(b'Y') => {
                    last_edit_kind = KIND_NONE;
                    let buf = &mut buffers[active];
                    if !buf.redo_group() {
                        status_msg.push_str("nothing to redo");
                    }
                    buf.group_counter += 1;
                }
                Key::Char(c) => {
                    let mut enc = [0u8; 4];
                    let s = c.encode_utf8(&mut enc);
                    do_edit(&mut buffers[active], s.as_bytes(), KIND_INSERT, &mut last_edit_kind, &mut status_msg, &mut scratch);
                }
                Key::Enter => {
                    do_edit(&mut buffers[active], b"\n", KIND_INSERT, &mut last_edit_kind, &mut status_msg, &mut scratch)
                }
                Key::Tab => {
                    do_edit(&mut buffers[active], b"\t", KIND_INSERT, &mut last_edit_kind, &mut status_msg, &mut scratch)
                }
                Key::Backspace => {
                    do_delete(&mut buffers[active], false, &mut last_edit_kind, &mut status_msg, &mut scratch)
                }
                Key::Delete => {
                    do_delete(&mut buffers[active], true, &mut last_edit_kind, &mut status_msg, &mut scratch)
                }
                Key::Esc => {
                    last_edit_kind = KIND_NONE;
                    buffers[active].sel_anchor = None;
                }
                _ => {
                    last_edit_kind = KIND_NONE;
                    let buf = &mut buffers[active];
                    let (base, shifted) = base_key(k);
                    if shifted {
                        if buf.sel_anchor.is_none() {
                            buf.sel_anchor = Some(buf.cursor);
                        }
                    } else {
                        buf.sel_anchor = None;
                    }
                    buf.group_counter += 1;
                    apply_movement(buf, base, editor_rows, &mut scratch, &mut starts_scratch);
                }
            }
        }

        // cooperative work slice (spec §18): runs only when input is idle
        if !term::poll_stdin(None, 0)?.0 {
            if let Some(s) = &mut search_job {
                let buf = &mut buffers[active];
                match s.step(buf, SLICE, &mut job_scratch) {
                    search::Step::Found(at) => {
                        let n = s.needle.len() as u64;
                        search_job = None;
                        if mode == Mode::ReplaceConfirm && at + n > replace_scope_end {
                            // match starts past the selection scope: done
                            buf.sel_anchor = None;
                            status_msg.clear();
                            let _ = write!(status_msg, "replaced {replace_count} — end of selection");
                            mode = Mode::Edit;
                        } else {
                            buf.sel_anchor = Some(at);
                            buf.cursor = at + n;
                            update_goal(buf, &mut job_scratch);
                            if mode == Mode::ReplaceConfirm {
                                confirm_match = Some(at);
                            }
                        }
                    }
                    search::Step::NotFound => {
                        search_job = None;
                        if mode == Mode::ReplaceConfirm {
                            buf.sel_anchor = None;
                            status_msg.clear();
                            let _ = write!(status_msg, "replaced {replace_count} — no more matches");
                            mode = Mode::Edit;
                        } else {
                            status_msg.clear();
                            status_msg.push_str("not found");
                        }
                    }
                    search::Step::Running => {}
                }
                dirty = true;
            } else if let Some(j) = &mut replace_job {
                let buf = &mut buffers[active];
                match j.step(buf, SLICE, &mut job_scratch) {
                    Ok(true) => {
                        status_msg.clear();
                        let _ = write!(status_msg, "replaced {}", replace_count + j.count);
                        replace_job = None;
                        mode = Mode::Edit;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        status_msg.clear();
                        status_msg.push_str(e);
                        replace_job = None;
                        mode = Mode::Edit;
                    }
                }
                dirty = true;
            } else if buffers[active].index_build.is_some() {
                buffers[active].step_index_build(4 * 1024 * 1024, &mut job_scratch);
                dirty = true;
            }
        }

        if dirty && !term::poll_stdin(None, 0)?.0 {
            let buf = &mut buffers[active];
            let cursor_screen = build_view(
                buf,
                editor_rows as usize,
                cols as usize,
                &mut row_store,
                &mut row_sel,
                &mut scratch,
                &mut starts_scratch,
            );
            format_status(buf, &mut status_left, &mut status_right);
            match mode {
                Mode::ConfirmQuit => {
                    status_left.clear();
                    status_left.push_str(" Unsaved changes — y: quit  s: save all & quit  n: back");
                }
                Mode::Prompt(PromptKind::Find) => {
                    status_left.clear();
                    let _ = write!(status_left, " Find: {prompt}");
                }
                Mode::Prompt(PromptKind::ReplaceNeedle) => {
                    status_left.clear();
                    let _ = write!(status_left, " Replace: {prompt}");
                }
                Mode::Prompt(PromptKind::ReplaceWith) => {
                    status_left.clear();
                    let _ = write!(
                        status_left,
                        " Replace {} with: {prompt}",
                        String::from_utf8_lossy(&find_needle)
                    );
                }
                Mode::ReplaceConfirm if confirm_match.is_some() => {
                    status_left.clear();
                    status_left.push_str(" Replace? y: yes  n: skip  a: all  Esc: stop");
                }
                _ => {
                    if let Some(s) = &search_job {
                        let _ = write!(status_left, "  searching… {}%", s.progress(buf.len()));
                    } else if let Some(j) = &replace_job {
                        let _ = write!(status_left, "  replacing… {}%", j.progress(buf.len()));
                    } else if let Some(ib) = &buf.index_build {
                        let pct = if buf.len() == 0 { 100 } else { ib.pos * 100 / buf.len() };
                        let _ = write!(status_left, "  indexing… {pct}%");
                    }
                    if !status_msg.is_empty() {
                        let _ = write!(status_left, "  — {status_msg}");
                    }
                }
            }
            // rebuild the owned tab snapshot only on change (no per-paint alloc)
            let tabs_stale = tabs_buf.len() != buffers.len()
                || tabs_buf
                    .iter()
                    .zip(buffers.iter())
                    .any(|(t, b)| t.0 != b.name || t.1 != b.modified());
            if tabs_stale {
                tabs_buf.clear();
                tabs_buf.extend(buffers.iter().map(|b| (b.name.clone(), b.modified())));
            }
            let v = View {
                cols,
                rows,
                tabs: &tabs_buf,
                active_tab: active,
                row_bytes: &row_store,
                row_sel: &row_sel,
                left_col: buffers[active].left_col,
                cursor_screen,
                status_left: &status_left,
                status_right: &status_right,
            };
            render::paint(&mut frame, &mut render_cache, &v);
            out.write_all(frame.as_bytes())?;
            out.flush()?;
            dirty = false;
            #[cfg(feature = "perf")]
            perf.frame_end(frame.as_bytes().len());
        }
    }
}

/// Dev-only frame timing + allocation counting (spec §20: perf tracing in
/// dev builds only). Build with `--features perf`, set TEDDY_PERF=<path>.
#[cfg(feature = "perf")]
mod perf {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static ALLOCS: AtomicU64 = AtomicU64::new(0);

    struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.realloc(p, l, n) }
        }
    }

    #[global_allocator]
    static A: Counting = Counting;

    pub struct Perf {
        log: Option<std::fs::File>,
        t0: Option<Instant>,
        allocs0: u64,
    }

    impl Perf {
        pub fn from_env() -> Self {
            let log = std::env::var_os("TEDDY_PERF").and_then(|p| std::fs::File::create(p).ok());
            Perf { log, t0: None, allocs0: 0 }
        }

        pub fn frame_start(&mut self) {
            self.t0 = Some(Instant::now());
            self.allocs0 = ALLOCS.load(Ordering::Relaxed);
        }

        pub fn frame_end(&mut self, frame_bytes: usize) {
            let (Some(t0), Some(log)) = (self.t0.take(), self.log.as_mut()) else { return };
            let us = t0.elapsed().as_micros();
            let allocs = ALLOCS.load(Ordering::Relaxed) - self.allocs0;
            let _ = writeln!(log, "{us} {allocs} {frame_bytes}");
        }
    }
}

// ---------------------------------------------------------------- editing

fn do_edit(
    buf: &mut Buffer,
    bytes: &[u8],
    kind: u8,
    last_kind: &mut u8,
    msg: &mut String,
    scratch: &mut Vec<u8>,
) {
    if kind != *last_kind {
        buf.group_counter += 1;
        *last_kind = kind;
    }
    let (s, e) = buf.selection().unwrap_or((buf.cursor, buf.cursor));
    let g = buf.group_counter;
    match buf.replace(s, e, bytes, g) {
        Ok(()) => {
            buf.cursor = s + bytes.len() as u64;
            buf.sel_anchor = None;
            update_goal(buf, scratch);
        }
        Err(er) => msg.push_str(er),
    }
}

fn do_delete(buf: &mut Buffer, forward: bool, last_kind: &mut u8, msg: &mut String, scratch: &mut Vec<u8>) {
    let kind = if forward { KIND_DELETE } else { KIND_BACKSPACE };
    if kind != *last_kind {
        buf.group_counter += 1;
        *last_kind = kind;
    }
    let (s, e) = match buf.selection() {
        Some(r) => r,
        None if forward => (buf.cursor, buf.next_boundary(buf.cursor)),
        None => (buf.prev_boundary(buf.cursor), buf.cursor),
    };
    if s == e {
        return;
    }
    let g = buf.group_counter;
    match buf.replace(s, e, b"", g) {
        Ok(()) => {
            buf.cursor = s;
            buf.sel_anchor = None;
            update_goal(buf, scratch);
        }
        Err(er) => msg.push_str(er),
    }
}

fn base_key(k: Key) -> (Key, bool) {
    match k {
        Key::SUp => (Key::Up, true),
        Key::SDown => (Key::Down, true),
        Key::SLeft => (Key::Left, true),
        Key::SRight => (Key::Right, true),
        Key::SHome => (Key::Home, true),
        Key::SEnd => (Key::End, true),
        _ => (k, false),
    }
}

// --------------------------------------------------------------- movement

fn line_slice(buf: &mut Buffer, line: u64, out: &mut Vec<u8>) {
    let s = buf.line_start(line);
    let e = buf.line_end(line);
    out.clear();
    buf.read_range(s, (e - s).min(LINE_CAP as u64), out);
}

fn update_goal(buf: &mut Buffer, scratch: &mut Vec<u8>) {
    if buf.line_index.is_none() {
        return;
    }
    let l = buf.line_of_byte(buf.cursor);
    let start = buf.line_start(l);
    line_slice(buf, l, scratch);
    buf.goal_col = render::visual_col(scratch, (buf.cursor - start) as usize);
}

/// Returns true if anything changed (cursor or viewport).
fn apply_movement(
    buf: &mut Buffer,
    key: Key,
    editor_rows: u64,
    scratch: &mut Vec<u8>,
    starts: &mut Vec<u64>,
) -> bool {
    let before = (buf.cursor, buf.top_line, buf.top_byte);
    if buf.huge || buf.line_index.is_none() {
        huge_movement(buf, key, editor_rows, scratch, starts);
    } else {
        normal_movement(buf, key, editor_rows, scratch);
    }
    (buf.cursor, buf.top_line, buf.top_byte) != before
}

fn normal_movement(buf: &mut Buffer, key: Key, editor_rows: u64, scratch: &mut Vec<u8>) {
    let line = buf.line_of_byte(buf.cursor);
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
fn huge_movement(
    buf: &mut Buffer,
    key: Key,
    editor_rows: u64,
    scratch: &mut Vec<u8>,
    starts: &mut Vec<u64>,
) {
    window_row_starts(buf, editor_rows as usize + 1, scratch, starts);
    let starts = &*starts;
    let row_of = |c: u64| starts.iter().rposition(|&s| s <= c).unwrap_or(0);
    match key {
        Key::Left => buf.cursor = buf.prev_boundary(buf.cursor),
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

/// Fill `starts` with up to `n` display-row starts beginning at top_byte.
fn window_row_starts(buf: &mut Buffer, n: usize, scratch: &mut Vec<u8>, starts: &mut Vec<u64>) {
    scratch.clear();
    let take = HUGE_WINDOW.min(buf.len().saturating_sub(buf.top_byte));
    buf.read_range(buf.top_byte, take, scratch);
    starts.clear();
    starts.push(buf.top_byte);
    for (i, &b) in scratch.iter().enumerate() {
        if starts.len() >= n {
            break;
        }
        if b == b'\n' {
            starts.push(buf.top_byte + i as u64 + 1);
        }
    }
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

/// Scroll to keep the cursor visible, fill row_store/row_sel, return the
/// screen cursor position.
fn build_view(
    buf: &mut Buffer,
    editor_rows: usize,
    cols: usize,
    row_store: &mut Vec<Vec<u8>>,
    row_sel: &mut Vec<Option<(usize, usize)>>,
    scratch: &mut Vec<u8>,
    starts: &mut Vec<u64>,
) -> (u16, u16) {
    row_store.iter_mut().for_each(|r| r.clear());
    while row_store.len() < editor_rows {
        row_store.push(Vec::new());
    }
    row_store.truncate(editor_rows);
    row_sel.clear();
    row_sel.resize(editor_rows, None);
    let sel = buf.selection();

    if buf.huge || buf.line_index.is_none() {
        window_row_starts(buf, editor_rows + 1, scratch, starts);
        let mut r = starts.iter().rposition(|&s| s <= buf.cursor).unwrap_or(0);
        // cursor left the visible rows (below): rebase window on its line
        if r >= editor_rows || buf.cursor > buf.top_byte + HUGE_WINDOW {
            buf.top_byte = prev_line_start(buf, (buf.cursor + 1).min(buf.len()), scratch);
            window_row_starts(buf, editor_rows + 1, scratch, starts);
            r = starts.iter().rposition(|&s| s <= buf.cursor).unwrap_or(0);
        }
        let r = r.min(editor_rows - 1);
        for (i, slot) in row_store.iter_mut().enumerate() {
            if let Some(&s) = starts.get(i) {
                let end = starts
                    .get(i + 1)
                    .map(|e| e - 1)
                    .unwrap_or_else(|| (s + LINE_CAP as u64).min(buf.len()));
                buf.read_range(s, (end - s).min(LINE_CAP as u64), slot);
                row_sel[i] = intersect_sel(sel, s, slot.len());
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
            row_sel[i] = intersect_sel(sel, s, slot.len());
        }
    }
    let line_start = buf.line_start(cursor_line);
    scratch.clear();
    buf.read_range(line_start, (buf.cursor - line_start).min(LINE_CAP as u64), scratch);
    let vcol = render::visual_col(scratch, scratch.len());
    clamp_left(buf, vcol, cols);
    ((vcol - buf.left_col) as u16, (cursor_line - buf.top_line) as u16)
}

fn intersect_sel(sel: Option<(u64, u64)>, row_start: u64, row_len: usize) -> Option<(usize, usize)> {
    let (a, b) = sel?;
    let lo = a.max(row_start);
    let hi = b.min(row_start + row_len as u64);
    if lo < hi {
        Some(((lo - row_start) as usize, (hi - row_start) as usize))
    } else {
        None
    }
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
    if buf.modified() {
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
    if buf.follow {
        left.push_str("  FOLLOW");
    }
    if buf.external_change {
        left.push_str("  CHG");
    }
    if buf.huge || buf.line_index.is_none() {
        let pct = if buf.len() == 0 { 100 } else { buf.cursor * 100 / buf.len() };
        let _ = write!(right, "byte {} / {} ({}%)  Ctrl+Q quit ", buf.cursor, buf.len(), pct);
    } else {
        let line = buf.line_of_byte(buf.cursor);
        let _ = write!(right, "Ln {}, Col {}  Ctrl+Q quit ", line + 1, buf.goal_col + 1);
    }
}
