//! Raw-mode terminal backend: termios, alternate screen, size, poll/read.

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::termios::{self, OptionalActions, Termios};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Original termios, kept for the panic hook — restore must work from
/// anywhere, including after `RawGuard` was leaked by a panic unwind.
static ORIG: OnceLock<Termios> = OnceLock::new();

/// Set while raw mode + alternate screen are active, so restore runs once:
/// a second `?1049l` (panic hook, then guard drop on unwind) would DECRC the
/// cursor back over the crash report.
static ENTERED: AtomicBool = AtomicBool::new(false);

pub struct RawGuard(());

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Enter raw mode + alternate screen. Fails on non-tty stdin/stdout.
pub fn enter() -> io::Result<RawGuard> {
    let stdin = rustix::stdio::stdin();
    if !termios::isatty(stdin) || !termios::isatty(rustix::stdio::stdout()) {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "not a terminal"));
    }
    let orig = termios::tcgetattr(stdin)?;
    let _ = ORIG.set(orig.clone());
    let mut raw = orig;
    raw.make_raw();
    termios::tcsetattr(stdin, OptionalActions::Flush, &raw)?;
    ENTERED.store(true, Ordering::SeqCst);
    // guard exists from this point: if the writes below fail, the early
    // return drops it and termios is restored (stdout's lock is reentrant)
    let guard = RawGuard(());
    let mut out = io::stdout().lock();
    out.write_all(b"\x1b[?1049h\x1b[2J\x1b[H")?;
    out.flush()?;
    Ok(guard)
}

/// Restore terminal state. Idempotent; safe to call from a panic hook.
pub fn restore() {
    // escapes once; termios every time, so a failed restore can be retried
    if ENTERED.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
    }
    if let Some(orig) = ORIG.get() {
        let _ = termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Flush, orig);
    }
}

/// (cols, rows) of the controlling terminal.
pub fn size() -> (u16, u16) {
    match termios::tcgetwinsize(rustix::stdio::stdout()) {
        // 0 means the pty hasn't been sized; treat like a failed query
        Ok(ws) if ws.ws_col > 0 && ws.ws_row > 1 => (ws.ws_col, ws.ws_row),
        _ => (80, 24),
    }
}

/// Poll stdin and extra fds for readability.
/// Returns stdin readiness and fills `ready` with per-extra-fd readiness.
pub fn poll_stdin(
    extra: &[std::os::fd::BorrowedFd],
    ready: &mut Vec<bool>,
    timeout_ms: u64,
) -> io::Result<bool> {
    let ts = Timespec {
        tv_sec: (timeout_ms / 1000) as _,
        tv_nsec: ((timeout_ms % 1000) * 1_000_000) as _,
    };
    let stdin = rustix::stdio::stdin();
    let mut fds = Vec::with_capacity(extra.len() + 1);
    fds.push(PollFd::new(&stdin, PollFlags::IN));
    for fd in extra {
        fds.push(PollFd::new(fd, PollFlags::IN));
    }
    ready.clear();
    ready.resize(extra.len(), false);
    match rustix::event::poll(&mut fds, Some(&ts)) {
        // HUP/ERR also count as ready: the read path owns EOF/errors
        Ok(_) => {
            let stdin_ready = !fds[0].revents().is_empty();
            for (slot, fd) in ready.iter_mut().zip(fds.iter().skip(1)) {
                *slot = !fd.revents().is_empty();
            }
            Ok(stdin_ready)
        }
        Err(rustix::io::Errno::INTR) => Ok(false), // e.g. SIGWINCH; size is re-checked each tick
        Err(e) => Err(e.into()),
    }
}

/// Read available bytes from stdin. 0 means EOF; EINTR is retried since
/// poll already reported readability.
pub fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match rustix::io::read(rustix::stdio::stdin(), &mut *buf) {
            Ok(n) => return Ok(n),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Largest copy also sent to the system clipboard. Terminals and tmux cap
/// OSC 52 payloads; past this only the internal register holds the copy.
pub const OSC52_CAP: usize = 256 * 1024;

/// OSC 52 write: hand the system clipboard `data` (base64, BEL-terminated).
/// Write-only by design — paste comes from the register or bracketed paste.
pub fn osc52(out: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let mut b64 = Vec::with_capacity(data.len().div_ceil(3) * 4 + 8);
    b64.extend_from_slice(b"\x1b]52;c;");
    base64(data, &mut b64);
    b64.push(0x07);
    out.write_all(&b64)
}

fn base64(data: &[u8], out: &mut Vec<u8>) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for c in data.chunks(3) {
        let b = |i: usize| c.get(i).copied().unwrap_or(0) as u32;
        let n = b(0) << 16 | b(1) << 8 | b(2);
        out.push(T[(n >> 18) as usize & 63]);
        out.push(T[(n >> 12) as usize & 63]);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63]
        } else {
            b'='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63]
        } else {
            b'='
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            let mut v = Vec::new();
            super::base64(i.as_bytes(), &mut v);
            assert_eq!(v, o.as_bytes());
        }
        let mut v = Vec::new();
        super::osc52(&mut v, b"hi").unwrap();
        assert_eq!(v, b"\x1b]52;c;aGk=\x07");
    }
}
