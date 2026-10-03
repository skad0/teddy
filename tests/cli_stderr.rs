//! Startup errors print user-supplied paths; hostile bytes must arrive escaped.
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

fn stderr_for(arg: &std::path::Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_teddy"))
        .arg(arg)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        !out.stderr.contains(&0x1b),
        "raw ESC on stderr: {:?}",
        out.stderr
    );
    String::from_utf8(out.stderr).unwrap()
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("teddy-cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn directory_arg_error_escapes_path() {
    let d = scratch("dir");
    let evil = d.join(OsStr::from_bytes(b"ev\x1b[2J\xc2\x9b"));
    std::fs::create_dir(&evil).unwrap();
    let err = stderr_for(&evil);
    assert!(
        err.contains("ev\\x1B[2J\\xC2\\x9B: is a directory"),
        "{err}"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn open_error_escapes_path() {
    // a socket exists but cannot be opened (ENXIO), reaching the open-error sink
    let d = scratch("sock");
    let evil = d.join(OsStr::from_bytes(b"s\x1b]0;x\x07"));
    let _l = std::os::unix::net::UnixListener::bind(&evil).unwrap();
    let err = stderr_for(&evil);
    assert!(err.contains("s\\x1B]0;x\\x07: "), "{err}");
    std::fs::remove_dir_all(&d).unwrap();
}
