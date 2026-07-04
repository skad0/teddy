#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod imp {
    use rustix::event::kqueue::{self, Event, EventFilter, EventFlags, VnodeEvents};
    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::{self, Mode, OFlags};
    use std::collections::HashMap;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::path::Path;
    use std::time::Duration;

    pub struct Watcher {
        kq: OwnedFd,
        files: HashMap<u64, OwnedFd>,
    }

    impl Watcher {
        pub fn new() -> io::Result<Watcher> {
            Ok(Watcher {
                kq: kqueue::kqueue()?,
                files: HashMap::new(),
            })
        }

        pub fn watch(&mut self, token: u64, path: &Path) -> io::Result<()> {
            self.unwatch(token);
            let file = fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())?;
            let events = VnodeEvents::WRITE
                | VnodeEvents::EXTEND
                | VnodeEvents::RENAME
                | VnodeEvents::DELETE
                | VnodeEvents::ATTRIBUTES;
            let change = Event::new(
                EventFilter::Vnode {
                    vnode: file.as_raw_fd(),
                    flags: events,
                },
                EventFlags::ADD | EventFlags::CLEAR,
                token as usize as *mut _,
            );

            // SAFETY: `file` is stored in `self.files`, so the registered fd
            // outlives the kqueue entry until re-watch/unwatch/drop.
            let mut none: [Event; 0] = [];
            unsafe { kqueue::kevent(&self.kq, &[change], &mut none, Some(Duration::ZERO))? };
            self.files.insert(token, file);
            Ok(())
        }

        pub fn unwatch(&mut self, token: u64) {
            self.files.remove(&token);
        }

        pub fn fd(&self) -> BorrowedFd<'_> {
            self.kq.as_fd()
        }

        pub fn drain(&mut self, out: &mut Vec<u64>) {
            let start = out.len();
            // ponytail: bounded batch; the main loop will call again if the fd
            // remains readable.
            let mut events = [MaybeUninit::uninit(); 64];

            loop {
                // SAFETY: all registered vnode fds are owned by `self.files`.
                let (got, _) = match unsafe {
                    kqueue::kevent(&self.kq, &[], &mut events, Some(Duration::ZERO))
                } {
                    Ok(got) => got,
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => break,
                };
                if got.is_empty() {
                    break;
                }
                for event in got {
                    let token = event.udata() as usize as u64;
                    if self.files.contains_key(&token) && !out[start..].contains(&token) {
                        out.push(token);
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
    use std::collections::HashMap;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::BorrowedFd;
    use std::path::Path;

    pub struct Watcher {
        inot: OwnedFd,
        by_token: HashMap<u64, i32>,
        by_wd: HashMap<i32, u64>,
    }

    impl Watcher {
        pub fn new() -> io::Result<Watcher> {
            Ok(Watcher {
                inot: inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)?,
                by_token: HashMap::new(),
                by_wd: HashMap::new(),
            })
        }

        pub fn watch(&mut self, token: u64, path: &Path) -> io::Result<()> {
            self.unwatch(token);
            let flags = WatchFlags::MODIFY
                | WatchFlags::CLOSE_WRITE
                | WatchFlags::MOVE_SELF
                | WatchFlags::DELETE_SELF
                | WatchFlags::ATTRIB;
            let wd = inotify::add_watch(&self.inot, path, flags)?;
            self.by_token.insert(token, wd);
            self.by_wd.insert(wd, token);
            Ok(())
        }

        pub fn unwatch(&mut self, token: u64) {
            if let Some(wd) = self.by_token.remove(&token) {
                self.by_wd.remove(&wd);
                let _ = inotify::remove_watch(&self.inot, wd);
            }
        }

        pub fn fd(&self) -> BorrowedFd<'_> {
            self.inot.as_fd()
        }

        pub fn drain(&mut self, out: &mut Vec<u64>) {
            let start = out.len();
            // ponytail: fixed drain buffer; inotify preserves unread events for
            // the next main-loop tick if a burst exceeds this.
            let mut buf = [MaybeUninit::uninit(); 4096];
            let mut reader = inotify::Reader::new(&self.inot, &mut buf);

            loop {
                match reader.next() {
                    Ok(event) => {
                        if let Some(&token) = self.by_wd.get(&event.wd()) {
                            if !out[start..].contains(&token) {
                                out.push(token);
                            }
                        }
                    }
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(rustix::io::Errno::WOULDBLOCK) => break,
                    Err(rustix::io::Errno::AGAIN) => break,
                    Err(_) => break,
                }
            }
        }
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
mod imp {
    use std::io;
    use std::os::fd::BorrowedFd;
    use std::path::Path;

    pub struct Watcher;

    impl Watcher {
        pub fn new() -> io::Result<Watcher> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file watching is unsupported on this platform",
            ))
        }

        pub fn watch(&mut self, _token: u64, _path: &Path) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file watching is unsupported on this platform",
            ))
        }

        pub fn unwatch(&mut self, _token: u64) {}

        pub fn fd(&self) -> BorrowedFd<'_> {
            panic!("file watching is unsupported on this platform")
        }

        pub fn drain(&mut self, _out: &mut Vec<u64>) {}
    }
}

pub use imp::Watcher;

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::Watcher;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn reports_append() {
        let mut path = std::env::temp_dir();
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("teddy-watch-{n}"));
        fs::write(&path, b"start").unwrap();

        let mut watcher = Watcher::new().unwrap();
        watcher.watch(7, &path).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"more")
            .unwrap();

        let mut out = Vec::new();
        for _ in 0..20 {
            watcher.drain(&mut out);
            if out.contains(&7) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = fs::remove_file(&path);
        assert!(out.contains(&7));
    }
}
