//! Local data foundation for the optional plugin manager.
//!
//! This module deliberately does not invoke Git, use the network, or spawn a
//! plugin. It only validates manager data and provides bounded local storage.

use rustix::fs::{flock, FlockOperation};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MAX_BYTES: usize = 64 * 1024;
const MAX_RECORDS: usize = 1024;
const MAX_ID: usize = 64;
const MAX_COMMIT: usize = 40;
const MAX_RELATIVE_PATH: usize = 4096;
const MAX_GIT_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_TREE_RECORDS: usize = 16 * 1024;
const MAX_TREE_PATH: usize = 4096;
const MAX_AGGREGATE_BYTES: u64 = 256 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OBJECT_ID: usize = 40;
const MAX_NONCE: usize = 20;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum ManagerError {
    Io(io::Error),
    Invalid(&'static str),
    Duplicate(&'static str),
    Unknown(&'static str),
    Missing(&'static str),
    Limit(&'static str),
    Ambiguous(&'static str),
    LockBusy,
    Git(&'static str),
    Timeout,
    Recovery(&'static str),
}
impl fmt::Display for ManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "manager data I/O: {e}"),
            Self::Invalid(s) => write!(f, "invalid manager {s}"),
            Self::Duplicate(s) => write!(f, "duplicate manager {s}"),
            Self::Unknown(s) => write!(f, "unknown manager {s}"),
            Self::Missing(s) => write!(f, "missing manager {s}"),
            Self::Limit(s) => write!(f, "manager {s} exceeds limit"),
            Self::Ambiguous(s) => write!(f, "ambiguous manager {s}"),
            Self::LockBusy => f.write_str("manager state is locked"),
            Self::Git(s) => write!(f, "manager git {s}"),
            Self::Timeout => f.write_str("manager Git operation timed out"),
            Self::Recovery(s) => write!(f, "manager recovery {s}"),
        }
    }
}
impl std::error::Error for ManagerError {}
impl From<io::Error> for ManagerError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerPaths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
}
impl ManagerPaths {
    pub fn from_env() -> Result<Self, ManagerError> {
        Self::from_values(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        )
    }
    pub fn from_values(
        config_home: Option<PathBuf>,
        data_home: Option<PathBuf>,
        state_home: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Result<Self, ManagerError> {
        let home = home.ok_or(ManagerError::Missing("HOME"))?;
        let config = config_home
            .unwrap_or_else(|| home.join(".config"))
            .join("teddy/plugin-manager");
        let data = data_home
            .unwrap_or_else(|| home.join(".local/share"))
            .join("teddy/plugins");
        let state = state_home
            .unwrap_or_else(|| home.join(".local/state"))
            .join("teddy/plugin-manager");
        Ok(Self {
            config,
            data,
            state,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    pub id: String,
    pub repository: String,
    pub commit: String,
    pub executables: BTreeMap<String, PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub repository: String,
    pub commit: String,
    pub executables: BTreeMap<String, PathBuf>,
}

pub fn parse_manifest(bytes: &[u8]) -> Result<PluginManifest, ManagerError> {
    let fields = parse_fields(bytes, "teddy-plugin.v1")?;
    let manifest = make_descriptor(&fields)?;
    Ok(manifest)
}
pub fn parse_catalog(bytes: &[u8]) -> Result<Vec<CatalogEntry>, ManagerError> {
    if bytes.len() > MAX_BYTES {
        return Err(ManagerError::Limit("catalog"));
    }
    let mut entries = Vec::new();
    let mut ids = BTreeSet::new();
    for block in split_blocks(bytes)? {
        let fields = parse_fields(block, "teddy-catalog.v1")?;
        let d = make_descriptor(&fields)?;
        if !ids.insert(d.id.clone()) {
            return Err(ManagerError::Duplicate("catalog plugin ID"));
        }
        entries.push(CatalogEntry {
            id: d.id,
            repository: d.repository,
            commit: d.commit,
            executables: d.executables,
        });
        if entries.len() > MAX_RECORDS {
            return Err(ManagerError::Limit("catalog records"));
        }
    }
    if entries.is_empty() {
        return Err(ManagerError::Missing("catalog entries"));
    }
    Ok(entries)
}

pub fn validate_plugin_id(id: &str) -> Result<(), ManagerError> {
    let b = id.as_bytes();
    if b.is_empty()
        || b.len() > MAX_ID
        || id.starts_with("teddy.")
        || !b[0].is_ascii_lowercase() && !b[0].is_ascii_digit()
        || !b.iter().all(|x| {
            x.is_ascii_lowercase() || x.is_ascii_digit() || matches!(x, b'.' | b'_' | b'-')
        })
    {
        return Err(ManagerError::Invalid("plugin ID"));
    }
    Ok(())
}
pub fn canonical_github_url(value: &str) -> Result<String, ManagerError> {
    let rest = value
        .strip_prefix("https://github.com/")
        .ok_or(ManagerError::Invalid("GitHub URL"))?;
    let rest = rest
        .strip_suffix(".git")
        .ok_or(ManagerError::Invalid("GitHub URL"))?;
    let mut it = rest.split('/');
    let owner = it
        .next()
        .filter(|x| !x.is_empty())
        .ok_or(ManagerError::Invalid("GitHub owner"))?;
    let repo = it
        .next()
        .filter(|x| !x.is_empty())
        .ok_or(ManagerError::Invalid("GitHub repository"))?;
    if it.next().is_some()
        || owner.len() > 100
        || repo.len() > 100
        || !owner.bytes().all(valid_github_byte)
        || !repo.bytes().all(valid_github_byte)
    {
        return Err(ManagerError::Invalid("GitHub URL"));
    }
    Ok(format!("https://github.com/{owner}/{repo}.git"))
}
pub fn validate_commit(value: &str) -> Result<(), ManagerError> {
    if value.len() != MAX_COMMIT
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err(ManagerError::Invalid("commit"));
    }
    Ok(())
}

/// The only Git executable accepted by the manager.  The path is always
/// absolute; this prevents PATH changes between validation and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitExecutable(pub PathBuf);

pub fn resolve_git() -> Result<GitExecutable, ManagerError> {
    resolve_git_values(
        std::env::var_os("TEDDY_MANAGER_GIT").map(PathBuf::from),
        std::env::var_os("PATH").map(|v| std::env::split_paths(&v).collect()),
    )
}

pub fn resolve_git_values(
    override_path: Option<PathBuf>,
    path: Option<Vec<PathBuf>>,
) -> Result<GitExecutable, ManagerError> {
    let candidates = override_path.into_iter().chain(
        path.unwrap_or_default()
            .into_iter()
            .filter(|p| p.is_absolute())
            .map(|p| p.join("git")),
    );
    for candidate in candidates {
        if candidate.is_absolute() && is_executable(&candidate) {
            return Ok(GitExecutable(candidate));
        }
    }
    Err(ManagerError::Missing("safe Git executable"))
}

fn is_executable(path: &Path) -> bool {
    let Ok(m) = fs::metadata(path) else {
        return false;
    };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Construct a command with a completely explicit, non-interactive Git
/// environment. Callers must pass the fixed argv for the one permitted Git
/// operation.
pub fn git_command(git: &GitExecutable, args: &[&str]) -> Command {
    let mut c = Command::new(&git.0);
    c.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "http.followRedirects=false",
    ])
    .args(args)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_PAGER", "cat")
    .env("GIT_EDITOR", "true")
    .env("GIT_ASKPASS", "true")
    .env("GIT_SSH_COMMAND", "false")
    .env("GIT_PROTOCOL_FROM_USER", "0")
    .env("GIT_CONFIG_COUNT", "0")
    .env("GIT_TEMPLATE_DIR", "/dev/null")
    .env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
    .env("GIT_HTTP_LOW_SPEED_TIME", "30")
    .env("LC_ALL", "C")
    .env("LANG", "C");
    c
}

pub fn git_argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

fn run_git(git: &GitExecutable, cwd: &Path, args: &[&str]) -> Result<Output, ManagerError> {
    run_git_input(git, cwd, args, &[])
}

fn run_git_input(
    git: &GitExecutable,
    cwd: &Path,
    args: &[&str],
    input: &[u8],
) -> Result<Output, ManagerError> {
    run_git_bounded(git, cwd, args, input, GIT_TIMEOUT)
}

pub fn run_git_bounded(
    git: &GitExecutable,
    cwd: &Path,
    args: &[&str],
    input: &[u8],
    timeout: Duration,
) -> Result<Output, ManagerError> {
    let mut child = git_command(git, args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ManagerError::Git("could not execute"))?;
    if !input.is_empty() {
        let mut stdin = child
            .stdin
            .take()
            .ok_or(ManagerError::Git("missing Git stdin"))?;
        stdin.write_all(input)?;
    }
    drop(child.stdin.take());
    let stdout = child
        .stdout
        .take()
        .ok_or(ManagerError::Git("missing Git stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(ManagerError::Git("missing Git stderr"))?;
    let out_thread = thread::spawn(move || read_process_pipe(stdout));
    let err_thread = thread::spawn(move || read_process_pipe(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            return Err(ManagerError::Timeout);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = out_thread
        .join()
        .map_err(|_| ManagerError::Git("Git output"))??;
    let stderr = err_thread
        .join()
        .map_err(|_| ManagerError::Git("Git output"))??;
    let output = Output {
        status,
        stdout,
        stderr,
    };
    if !output.status.success() {
        return Err(ManagerError::Git("operation failed"));
    }
    Ok(output)
}

fn read_process_pipe(mut pipe: impl Read) -> Result<Vec<u8>, ManagerError> {
    let mut output = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if output.len().saturating_add(n) > MAX_GIT_OUTPUT {
            return Err(ManagerError::Limit("Git output"));
        }
        output.extend_from_slice(&buf[..n]);
    }
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRecord {
    pub mode: u32,
    pub kind: String,
    pub object_id: String,
    pub path: PathBuf,
}

pub fn parse_tree_records(bytes: &[u8]) -> Result<Vec<TreeRecord>, ManagerError> {
    if bytes.len() > MAX_GIT_OUTPUT {
        return Err(ManagerError::Limit("tree output"));
    }
    let mut out = Vec::new();
    for raw in bytes.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        if out.len() >= MAX_TREE_RECORDS {
            return Err(ManagerError::Limit("tree records"));
        }
        let tab = raw
            .iter()
            .position(|b| *b == b'\t')
            .ok_or(ManagerError::Invalid("tree record"))?;
        let head =
            std::str::from_utf8(&raw[..tab]).map_err(|_| ManagerError::Invalid("tree record"))?;
        let mut p = head.split_whitespace();
        let mode = u32::from_str_radix(p.next().ok_or(ManagerError::Invalid("tree mode"))?, 8)
            .map_err(|_| ManagerError::Invalid("tree mode"))?;
        let kind = p.next().ok_or(ManagerError::Invalid("tree type"))?;
        let object_id = p.next().ok_or(ManagerError::Invalid("tree object"))?;
        if p.next().is_some()
            || !matches!(kind, "blob" | "tree")
            || object_id.len() != MAX_OBJECT_ID
            || !object_id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ManagerError::Invalid("tree type"));
        }
        if mode == 0o120000 || mode == 0o160000 {
            return Err(ManagerError::Invalid("tree link"));
        }
        if kind == "blob" && !matches!(mode, 0o100644 | 0o100755) {
            return Err(ManagerError::Invalid("tree mode"));
        }
        if kind == "tree" && mode != 0o040000 {
            return Err(ManagerError::Invalid("tree mode"));
        }
        let path = &raw[tab + 1..];
        if path.is_empty() || path.len() > MAX_TREE_PATH || path.contains(&0) {
            return Err(ManagerError::Invalid("tree path"));
        }
        let text = std::str::from_utf8(path).map_err(|_| ManagerError::Invalid("tree path"))?;
        let pp = Path::new(text);
        if pp.is_absolute()
            || pp.components().any(|c| !matches!(c, Component::Normal(_)))
            || pp.components().any(|c| c.as_os_str() == ".git")
        {
            return Err(ManagerError::Invalid("tree path"));
        }
        out.push(TreeRecord {
            mode,
            kind: kind.to_owned(),
            object_id: object_id.to_owned(),
            path: pp.to_owned(),
        });
    }
    if out.is_empty() {
        return Err(ManagerError::Missing("tree records"));
    }
    Ok(out)
}

pub fn validate_payload_file(path: &Path) -> Result<(), ManagerError> {
    let m = fs::symlink_metadata(path).map_err(ManagerError::Io)?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(ManagerError::Invalid("payload file"));
    }
    if m.len() > 64 * 1024 * 1024 {
        return Err(ManagerError::Limit("payload"));
    }
    if !is_executable(path) {
        return Err(ManagerError::Invalid("payload executable"));
    }
    let mut f = File::open(path)?;
    let mut buf = [0u8; 128];
    let n = f.read(&mut buf)?;
    if n >= 5 && &buf[..5] == b"version https" {
        return Err(ManagerError::Invalid("LFS pointer"));
    }
    Ok(())
}

/// Remove only manager-owned, name-validated leftovers. This is deliberately
/// conservative: an unknown directory is never recursively deleted.
pub fn recover_stale(paths: &ManagerPaths) -> Result<(), ManagerError> {
    if !paths.data.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&paths.data)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if parse_manager_artifact_name(&name).is_some() && entry.file_type()?.is_dir() {
            safe_remove_dir(&paths.data, &entry.path())?;
        }
    }
    Ok(())
}

fn parse_manager_artifact_name(name: &str) -> Option<(&str, &str, bool)> {
    if let Some(rest) = name.strip_prefix(".trash-") {
        let (id, commit) = rest.rsplit_once('-')?;
        if validate_plugin_id(id).is_ok()
            && validate_commit(commit).is_ok()
            && format!(".trash-{id}-{commit}") == name
        {
            return Some((id, commit, false));
        }
        return None;
    }
    let rest = name.strip_prefix(".staging-")?;
    let mut parts = rest.rsplitn(3, '-');
    let nonce = parts.next()?;
    let commit = parts.next()?;
    let id = parts.next()?;
    if nonce.is_empty()
        || nonce.len() > MAX_NONCE
        || !nonce.bytes().all(|b| b.is_ascii_digit())
        || validate_plugin_id(id).is_err()
        || validate_commit(commit).is_err()
        || format!(".staging-{id}-{commit}-{nonce}") != name
    {
        return None;
    }
    Some((id, commit, true))
}

/// Install a pre-validated manifest from its pinned public repository. The
/// lock must be held by the caller for the whole operation. `manifest_path`
/// and `platform` are relative repository paths/manifest keys, respectively.
/// This function performs no command other than the fixed Git operations below.
pub fn install_git(
    paths: &ManagerPaths,
    _lock: &ManagerLock,
    expected: &PluginManifest,
    manifest_path: &str,
    platform: &str,
) -> Result<PathBuf, ManagerError> {
    validate_plugin_id(&expected.id)?;
    let repository = canonical_github_url(&expected.repository)?;
    validate_commit(&expected.commit)?;
    let manifest_path = validate_relative_executable(manifest_path)?;
    let selected = expected
        .executables
        .get(platform)
        .ok_or(ManagerError::Missing("platform executable"))?;
    let git = resolve_git()?;
    recover_manager(paths)?;
    ensure_mutation_allowed(paths)?;
    safe_create_dir_all(&paths.data, &paths.data)?;
    let mut nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut attempts = 0usize;
    let stage = loop {
        let candidate = staging_artifact_path(paths, &expected.id, &expected.commit, nonce)?;
        if !candidate.exists() {
            break candidate;
        }
        attempts += 1;
        if attempts >= 16 {
            return Err(ManagerError::Limit("staging collisions"));
        }
        nonce = nonce
            .checked_add(1)
            .ok_or(ManagerError::Limit("staging nonce"))?;
    };
    manager_path(&paths.data, &stage)?;
    let journal = paths.state.join("journal");
    write_journal_atomic(
        &journal,
        &JournalRecord {
            operation: "install".into(),
            id: expected.id.clone(),
            commit: expected.commit.clone(),
            phase: "staging".into(),
        },
    )?;
    fs::create_dir(&stage)?;
    sync_dir(&paths.data)?;
    let repo = stage.join("repo");
    let result = (|| {
        fs::create_dir(&repo)?;
        run_git(&git, &repo, &["init", "--"])?;
        run_git(&git, &repo, &["remote", "add", "origin", &repository])?;
        run_git(
            &git,
            &repo,
            &["fetch", "--depth=1", "origin", &expected.commit],
        )?;
        let rev = run_git(&git, &repo, &["rev-parse", "FETCH_HEAD^{commit}"])?;
        let got = std::str::from_utf8(&rev.stdout)
            .map_err(|_| ManagerError::Git("bad revision"))?
            .trim();
        if got != expected.commit {
            return Err(ManagerError::Git("commit mismatch"));
        }
        let tree = run_git(&git, &repo, &["ls-tree", "-rz", "-r", "FETCH_HEAD"])?;
        let records = parse_tree_records(&tree.stdout)?;
        let mut object_input = Vec::new();
        let mut unique = BTreeMap::new();
        for r in &records {
            if r.kind == "blob" && unique.insert(r.object_id.clone(), ()).is_none() {
                object_input.extend_from_slice(r.object_id.as_bytes());
                object_input.push(b'\n');
            }
        }
        let sizes = run_git_input(&git, &repo, &["cat-file", "--batch-check"], &object_input)?;
        let mut blob_sizes = BTreeMap::new();
        let mut aggregate = 0u64;
        for line in sizes
            .stdout
            .split(|b| *b == b'\n')
            .filter(|x| !x.is_empty())
        {
            let text =
                std::str::from_utf8(line).map_err(|_| ManagerError::Git("bad object metadata"))?;
            let mut fields = text.split_whitespace();
            let oid = fields
                .next()
                .ok_or(ManagerError::Git("bad object metadata"))?;
            let kind = fields
                .next()
                .ok_or(ManagerError::Git("bad object metadata"))?;
            let size: u64 = fields
                .next()
                .ok_or(ManagerError::Git("bad object metadata"))?
                .parse()
                .map_err(|_| ManagerError::Git("bad object size"))?;
            if fields.next().is_some()
                || kind != "blob"
                || oid.len() != MAX_OBJECT_ID
                || size > 64 * 1024 * 1024
            {
                return Err(ManagerError::Invalid("blob metadata"));
            }
            aggregate = aggregate
                .checked_add(size)
                .ok_or(ManagerError::Limit("aggregate payload"))?;
            if aggregate > MAX_AGGREGATE_BYTES {
                return Err(ManagerError::Limit("aggregate payload"));
            }
            blob_sizes.insert(oid.to_owned(), size);
        }
        if blob_sizes.len() != unique.len() {
            return Err(ManagerError::Git("incomplete object metadata"));
        }
        let mut materialized_bytes = 0u64;
        for record in &records {
            if record.kind == "blob" {
                let size = *blob_sizes
                    .get(&record.object_id)
                    .ok_or(ManagerError::Git("missing blob metadata"))?;
                materialized_bytes = materialized_bytes
                    .checked_add(size)
                    .ok_or(ManagerError::Limit("aggregate payload"))?;
                if materialized_bytes > MAX_AGGREGATE_BYTES {
                    return Err(ManagerError::Limit("aggregate payload"));
                }
            }
        }
        let mut manifest_count = 0;
        let mut payload_count = 0;
        for r in &records {
            if r.kind != "blob" || !matches!(r.mode, 0o100644 | 0o100755) {
                return Err(ManagerError::Invalid("tree payload"));
            }
            if !blob_sizes.contains_key(&r.object_id) {
                return Err(ManagerError::Git("missing blob metadata"));
            }
            if r.path == manifest_path {
                manifest_count += 1;
            }
            if r.path == *selected {
                payload_count += 1;
            }
        }
        if manifest_count != 1 {
            return Err(if manifest_count == 0 {
                ManagerError::Missing("manifest")
            } else {
                ManagerError::Ambiguous("manifest")
            });
        }
        if payload_count != 1 {
            return Err(if payload_count == 0 {
                ManagerError::Missing("payload")
            } else {
                ManagerError::Ambiguous("payload")
            });
        }
        run_git(
            &git,
            &repo,
            &["checkout", "--detach", "--force", &expected.commit],
        )?;
        let actual = parse_manifest(&read_bounded(&repo.join(&manifest_path))?)?;
        if actual != *expected {
            return Err(ManagerError::Git("manifest mismatch"));
        }
        let source = repo.join(selected);
        if blob_sizes
            .get(
                &records
                    .iter()
                    .find(|r| r.path == *selected)
                    .ok_or(ManagerError::Missing("payload"))?
                    .object_id,
            )
            .copied()
            .unwrap_or(u64::MAX)
            > 64 * 1024 * 1024
        {
            return Err(ManagerError::Limit("payload"));
        }
        validate_payload_file(&source)?;
        let payload = stage.join("payload");
        fs::copy(&source, &payload)?;
        validate_payload_file(&payload)?;
        let digest = payload_digest(&payload)?;
        // Never promote the checkout (and especially never promote its .git).
        fs::remove_dir_all(&repo)?;
        let version = paths.data.join(&expected.id).join(&expected.commit);
        manager_path(&paths.data, &version)?;
        if version.exists() {
            let vm = fs::symlink_metadata(&version)?;
            if !vm.is_dir() || vm.file_type().is_symlink() {
                return Err(ManagerError::Invalid("installed version"));
            }
            let rm = fs::symlink_metadata(version.join("receipt"))?;
            if !rm.is_file() || rm.file_type().is_symlink() {
                return Err(ManagerError::Invalid("installed receipt"));
            }
            let receipt = read_receipt(&version.join("receipt"))?;
            validate_installed(&version.join("payload"), &receipt, expected, Some(&digest))?;
            return Ok(version.join("payload"));
        }
        let receipt = Receipt {
            id: expected.id.clone(),
            commit: expected.commit.clone(),
            executable: PathBuf::from("payload"),
            digest: Some(digest),
        };
        write_journal_atomic(
            &journal,
            &JournalRecord {
                operation: "install".into(),
                id: expected.id.clone(),
                commit: expected.commit.clone(),
                phase: "promotion".into(),
            },
        )?;
        write_receipt_atomic(&stage.join("receipt"), &receipt)?;
        safe_create_dir_all(
            &paths.data,
            version
                .parent()
                .ok_or(ManagerError::Invalid("version path"))?,
        )?;
        fs::rename(&stage, &version)?;
        sync_dir(
            version
                .parent()
                .ok_or(ManagerError::Recovery("promotion parent"))?,
        )?;
        sync_dir(&paths.data)?;
        remove_journal(&journal)?;
        Ok(version.join("payload"))
    })();
    if result.is_err() {
        if stage.exists() {
            let _ = safe_remove_dir(&paths.data, &stage);
        }
    }
    result
}

fn validate_installed(
    payload: &Path,
    receipt: &Receipt,
    expected: &PluginManifest,
    fresh_digest: Option<&str>,
) -> Result<(), ManagerError> {
    if receipt.id != expected.id
        || receipt.commit != expected.commit
        || receipt.executable != PathBuf::from("payload")
        || receipt.digest.is_none()
    {
        return Err(ManagerError::Invalid("installed receipt"));
    }
    validate_payload_file(payload)?;
    let existing = payload_digest(payload)?;
    if receipt.digest.as_deref() != Some(existing.as_str())
        || fresh_digest != Some(existing.as_str())
    {
        return Err(ManagerError::Invalid("installed payload digest"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateIdentity {
    pub receipt: Receipt,
    pub payload: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStage {
    CandidateInstalled,
    OldQuiesced,
    CandidateEnableRequested,
    CandidateRunning,
    RollbackRequested,
    RollbackRunning,
    Completed,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateTransaction {
    pub id: String,
    pub old: UpdateIdentity,
    pub candidate: UpdateIdentity,
    pub stage: UpdateStage,
}

impl UpdateStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::CandidateInstalled => "candidate-installed",
            Self::OldQuiesced => "old-quiesced",
            Self::CandidateEnableRequested => "candidate-enable-requested",
            Self::CandidateRunning => "candidate-running",
            Self::RollbackRequested => "rollback-requested",
            Self::RollbackRunning => "rollback-running",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        }
    }
    fn parse(value: &str) -> Result<Self, ManagerError> {
        match value {
            "candidate-installed" => Ok(Self::CandidateInstalled),
            "old-quiesced" => Ok(Self::OldQuiesced),
            "candidate-enable-requested" => Ok(Self::CandidateEnableRequested),
            "candidate-running" => Ok(Self::CandidateRunning),
            "rollback-requested" => Ok(Self::RollbackRequested),
            "rollback-running" => Ok(Self::RollbackRunning),
            "completed" => Ok(Self::Completed),
            "aborted" => Ok(Self::Aborted),
            _ => Err(ManagerError::Invalid("update stage")),
        }
    }
}

fn update_identity(paths: &ManagerPaths, identity: &UpdateIdentity) -> Result<(), ManagerError> {
    validate_plugin_id(&identity.receipt.id)?;
    if identity.receipt.id == "teddy" {
        return Err(ManagerError::Invalid("manager self-update"));
    }
    validate_commit(&identity.receipt.commit)?;
    let digest = identity
        .receipt
        .digest
        .as_deref()
        .ok_or(ManagerError::Invalid("update receipt digest"))?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ManagerError::Invalid("update receipt digest"));
    }
    let payload = payload_path(paths, &identity.receipt.id, &identity.receipt.commit)?;
    if identity.payload != payload || identity.receipt.executable != PathBuf::from("payload") {
        return Err(ManagerError::Invalid("update payload location"));
    }
    manager_path(&paths.data, &identity.payload)?;
    validate_payload_file(&identity.payload)?;
    if payload_digest(&identity.payload)? != digest {
        return Err(ManagerError::Invalid("update payload digest"));
    }
    let version = identity
        .payload
        .parent()
        .ok_or(ManagerError::Invalid("update payload location"))?;
    let receipt_path = version.join("receipt");
    manager_path(&paths.data, &receipt_path)?;
    let metadata =
        fs::symlink_metadata(&receipt_path).map_err(|_| ManagerError::Invalid("update receipt"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ManagerError::Invalid("update receipt"));
    }
    if read_receipt(&receipt_path)? != identity.receipt {
        return Err(ManagerError::Invalid("update receipt mismatch"));
    }
    Ok(())
}

fn validate_update_transaction(
    paths: &ManagerPaths,
    transaction: &UpdateTransaction,
) -> Result<(), ManagerError> {
    validate_plugin_id(&transaction.id)?;
    if transaction.id != transaction.old.receipt.id
        || transaction.id != transaction.candidate.receipt.id
        || transaction.old.receipt.commit == transaction.candidate.receipt.commit
    {
        return Err(ManagerError::Invalid("update target mismatch"));
    }
    update_identity(paths, &transaction.old)?;
    update_identity(paths, &transaction.candidate)?;
    Ok(())
}

fn update_transition_allowed(from: UpdateStage, to: UpdateStage) -> bool {
    matches!(
        (from, to),
        (UpdateStage::CandidateInstalled, UpdateStage::OldQuiesced)
            | (
                UpdateStage::OldQuiesced,
                UpdateStage::CandidateEnableRequested
            )
            | (
                UpdateStage::CandidateEnableRequested,
                UpdateStage::CandidateRunning
            )
            | (UpdateStage::CandidateRunning, UpdateStage::Completed)
            | (UpdateStage::OldQuiesced, UpdateStage::RollbackRequested)
            | (
                UpdateStage::CandidateEnableRequested,
                UpdateStage::RollbackRequested
            )
            | (
                UpdateStage::CandidateRunning,
                UpdateStage::RollbackRequested
            )
            | (UpdateStage::RollbackRequested, UpdateStage::RollbackRunning)
            | (UpdateStage::RollbackRunning, UpdateStage::Completed)
            | (UpdateStage::CandidateInstalled, UpdateStage::Aborted)
            | (UpdateStage::OldQuiesced, UpdateStage::Aborted)
            | (UpdateStage::CandidateEnableRequested, UpdateStage::Aborted)
            | (UpdateStage::CandidateRunning, UpdateStage::Aborted)
            | (UpdateStage::RollbackRequested, UpdateStage::Aborted)
            | (UpdateStage::RollbackRunning, UpdateStage::Aborted)
    )
}

pub fn begin_update(
    paths: &ManagerPaths,
    _lock: &ManagerLock,
    old: UpdateIdentity,
    candidate: UpdateIdentity,
) -> Result<UpdateTransaction, ManagerError> {
    if pending_update(paths)?.is_some() {
        return Err(ManagerError::Duplicate("active update"));
    }
    let transaction = UpdateTransaction {
        id: old.receipt.id.clone(),
        old,
        candidate,
        stage: UpdateStage::CandidateInstalled,
    };
    validate_update_transaction(paths, &transaction)?;
    write_update_journal(paths, &transaction)?;
    Ok(transaction)
}

pub fn advance_update(
    paths: &ManagerPaths,
    _lock: &ManagerLock,
    transaction: &UpdateTransaction,
    next: UpdateStage,
) -> Result<UpdateTransaction, ManagerError> {
    let active = pending_update(paths)?.ok_or(ManagerError::Missing("active update"))?;
    if active != *transaction {
        return Err(ManagerError::Invalid("stale update transaction"));
    }
    if !update_transition_allowed(transaction.stage, next) {
        return Err(ManagerError::Invalid("update stage transition"));
    }
    validate_update_transaction(paths, transaction)?;
    let mut updated = transaction.clone();
    updated.stage = next;
    write_update_journal(paths, &updated)?;
    if matches!(next, UpdateStage::Completed | UpdateStage::Aborted) {
        remove_update_journal(paths)?;
    }
    Ok(updated)
}

pub fn pending_update(paths: &ManagerPaths) -> Result<Option<UpdateTransaction>, ManagerError> {
    let path = paths.state.join("update-journal");
    manager_path(&paths.state, &path)?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_update_journal(paths)?))
}

/// Reject ordinary manager mutations while the update transaction owns the
/// installed versions. The dedicated update APIs are the only operations
/// allowed to advance or finish that transaction.
pub fn ensure_mutation_allowed(paths: &ManagerPaths) -> Result<(), ManagerError> {
    if pending_update(paths)?.is_some() {
        return Err(ManagerError::Recovery("update transaction is active"));
    }
    Ok(())
}

fn update_journal_path(paths: &ManagerPaths) -> Result<PathBuf, ManagerError> {
    let path = paths.state.join("update-journal");
    manager_path(&paths.state, &path)?;
    Ok(path)
}

fn write_update_journal(
    paths: &ManagerPaths,
    transaction: &UpdateTransaction,
) -> Result<(), ManagerError> {
    validate_update_transaction(paths, transaction)?;
    let path = update_journal_path(paths)?;
    let data = format!(
        "format=teddy-update.v1\nid={}\nstage={}\nold.commit={}\nold.payload={}\nold.digest={}\ncandidate.commit={}\ncandidate.payload={}\ncandidate.digest={}\n",
        transaction.id,
        transaction.stage.as_str(),
        transaction.old.receipt.commit,
        transaction.old.payload.to_str().ok_or(ManagerError::Invalid("update path"))?,
        transaction.old.receipt.digest.as_deref().ok_or(ManagerError::Invalid("update digest"))?,
        transaction.candidate.receipt.commit,
        transaction.candidate.payload.to_str().ok_or(ManagerError::Invalid("update path"))?,
        transaction.candidate.receipt.digest.as_deref().ok_or(ManagerError::Invalid("update digest"))?,
    );
    atomic_write(&path, data.as_bytes())
}

fn read_update_journal(paths: &ManagerPaths) -> Result<UpdateTransaction, ManagerError> {
    let path = update_journal_path(paths)?;
    let fields = parse_fields(&read_bounded(&path)?, "teddy-update.v1")?;
    let old_commit = required(&fields, "old.commit")?.to_owned();
    let old_payload = PathBuf::from(required(&fields, "old.payload")?);
    let old_digest = required(&fields, "old.digest")?.to_owned();
    let candidate_commit = required(&fields, "candidate.commit")?.to_owned();
    let candidate_payload = PathBuf::from(required(&fields, "candidate.payload")?);
    let candidate_digest = required(&fields, "candidate.digest")?.to_owned();
    let id = required(&fields, "id")?.to_owned();
    let transaction = UpdateTransaction {
        id: id.clone(),
        old: UpdateIdentity {
            receipt: Receipt {
                id: id.clone(),
                commit: old_commit,
                executable: PathBuf::from("payload"),
                digest: Some(old_digest),
            },
            payload: old_payload,
        },
        candidate: UpdateIdentity {
            receipt: Receipt {
                id: id.clone(),
                commit: candidate_commit,
                executable: PathBuf::from("payload"),
                digest: Some(candidate_digest),
            },
            payload: candidate_payload,
        },
        stage: UpdateStage::parse(required(&fields, "stage")?)?,
    };
    validate_update_transaction(paths, &transaction)?;
    Ok(transaction)
}

fn remove_update_journal(paths: &ManagerPaths) -> Result<(), ManagerError> {
    let path = update_journal_path(paths)?;
    remove_journal(&path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalResult {
    Removed,
    NotFound,
}

/// Remove one immutable installation after verifying the exact receipt and
/// payload digest supplied by the UI. The lock is intentionally required in
/// the signature so callers cannot accidentally perform an unlocked
/// filesystem mutation.
pub fn remove_installed(
    paths: &ManagerPaths,
    _lock: &ManagerLock,
    expected: &Receipt,
) -> Result<RemovalResult, ManagerError> {
    validate_plugin_id(&expected.id)?;
    validate_commit(&expected.commit)?;
    if expected.digest.is_none() {
        return Err(ManagerError::Invalid("removal receipt digest"));
    }
    validate_relative_executable(
        expected
            .executable
            .to_str()
            .ok_or(ManagerError::Invalid("removal receipt path"))?,
    )?;
    recover_manager(paths)?;
    ensure_mutation_allowed(paths)?;
    let version = paths.data.join(&expected.id).join(&expected.commit);
    let trash = trash_path(paths, &expected.id, &expected.commit)?;
    manager_path(&paths.data, &version)?;
    manager_path(&paths.data, &trash)?;
    if !version.exists() {
        if trash.exists() {
            return Err(ManagerError::Recovery("unresolved removal trash"));
        }
        return Ok(RemovalResult::NotFound);
    }
    let version_meta = fs::symlink_metadata(&version)?;
    if !version_meta.is_dir() || version_meta.file_type().is_symlink() {
        return Err(ManagerError::Invalid("installed version"));
    }
    let receipt_path = version.join("receipt");
    manager_path(&paths.data, &receipt_path)?;
    let receipt_meta = fs::symlink_metadata(&receipt_path)
        .map_err(|_| ManagerError::Invalid("installed receipt"))?;
    if !receipt_meta.is_file() || receipt_meta.file_type().is_symlink() {
        return Err(ManagerError::Invalid("installed receipt"));
    }
    let actual = read_receipt(&receipt_path)?;
    if actual != *expected {
        return Err(ManagerError::Invalid("removal receipt mismatch"));
    }
    let payload = version.join(&actual.executable);
    manager_path(&paths.data, &payload)?;
    validate_payload_file(&payload)?;
    let digest = payload_digest(&payload)?;
    if actual.digest.as_deref() != Some(digest.as_str()) {
        return Err(ManagerError::Invalid("removal payload digest"));
    }

    let journal = paths.state.join("journal");
    let record = |phase: &str| JournalRecord {
        operation: "remove".into(),
        id: expected.id.clone(),
        commit: expected.commit.clone(),
        phase: phase.into(),
    };
    write_journal_atomic(&journal, &record("remove-intent"))?;
    if trash.exists() {
        return Err(ManagerError::Recovery("removal trash already exists"));
    }
    if let Err(error) = fs::rename(&version, &trash) {
        let _ = error;
        return Err(ManagerError::Recovery("removal move failed"));
    }
    sync_dir(
        version
            .parent()
            .ok_or(ManagerError::Recovery("removal source parent"))?,
    )?;
    sync_dir(&paths.data)?;
    write_journal_atomic(&journal, &record("remove-trash"))?;
    write_journal_atomic(&journal, &record("remove-delete"))?;
    if let Err(_error) = safe_remove_dir(&paths.data, &trash) {
        return Err(ManagerError::Recovery("removal trash retained"));
    }
    sync_dir(&paths.data)?;
    remove_journal(&journal)?;
    Ok(RemovalResult::Removed)
}
pub fn validate_relative_executable(value: &str) -> Result<PathBuf, ManagerError> {
    if value.is_empty() || value.len() > MAX_RELATIVE_PATH || value.contains('\\') {
        return Err(ManagerError::Invalid("executable path"));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ManagerError::Invalid("executable path"));
    }
    Ok(path.to_owned())
}
pub fn select_executable(
    entries: &[(String, String)],
    target: &str,
) -> Result<PathBuf, ManagerError> {
    let mut selected = None;
    for (name, path) in entries {
        if name == target {
            if selected.is_some() {
                return Err(ManagerError::Ambiguous("platform executable"));
            }
            selected = Some(validate_relative_executable(path)?);
        }
    }
    selected.ok_or(ManagerError::Missing("platform executable"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub id: String,
    pub commit: String,
    pub executable: PathBuf,
    pub digest: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRecord {
    pub id: String,
    pub commit: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    pub operation: String,
    pub id: String,
    pub commit: String,
    pub phase: String,
}

pub fn payload_path(paths: &ManagerPaths, id: &str, commit: &str) -> Result<PathBuf, ManagerError> {
    validate_plugin_id(id)?;
    validate_commit(commit)?;
    let path = paths.data.join(id).join(commit).join("payload");
    manager_path(&paths.data, &path)?;
    Ok(path)
}
pub fn staging_path(paths: &ManagerPaths, id: &str, commit: &str) -> Result<PathBuf, ManagerError> {
    validate_plugin_id(id)?;
    validate_commit(commit)?;
    let path = paths.data.join(format!(".staging-{id}-{commit}"));
    manager_path(&paths.data, &path)?;
    Ok(path)
}

fn staging_artifact_path(
    paths: &ManagerPaths,
    id: &str,
    commit: &str,
    nonce: u64,
) -> Result<PathBuf, ManagerError> {
    validate_plugin_id(id)?;
    validate_commit(commit)?;
    let text = nonce.to_string();
    if text.len() > MAX_NONCE {
        return Err(ManagerError::Limit("staging nonce"));
    }
    let path = paths.data.join(format!(".staging-{id}-{commit}-{text}"));
    manager_path(&paths.data, &path)?;
    Ok(path)
}
pub fn trash_path(paths: &ManagerPaths, id: &str, commit: &str) -> Result<PathBuf, ManagerError> {
    validate_plugin_id(id)?;
    validate_commit(commit)?;
    let path = paths.data.join(format!(".trash-{id}-{commit}"));
    manager_path(&paths.data, &path)?;
    Ok(path)
}

fn manager_path(root: &Path, path: &Path) -> Result<(), ManagerError> {
    if !root.is_absolute() || !path.is_absolute() || !path.starts_with(root) {
        return Err(ManagerError::Invalid("manager path"));
    }
    let mut ancestor = PathBuf::from("/");
    for component in root.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        ancestor.push(name);
        if let Ok(meta) = fs::symlink_metadata(&ancestor) {
            if meta.file_type().is_symlink() {
                return Err(ManagerError::Invalid("manager root symlink"));
            }
        }
    }
    let mut current = root.to_owned();
    if current.exists() && fs::symlink_metadata(&current)?.file_type().is_symlink() {
        return Err(ManagerError::Invalid("manager root symlink"));
    }
    for component in path
        .strip_prefix(root)
        .map_err(|_| ManagerError::Invalid("manager path"))?
        .components()
    {
        let Component::Normal(name) = component else {
            return Err(ManagerError::Invalid("manager path"));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ManagerError::Invalid("manager path symlink"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(ManagerError::Io(e)),
        }
    }
    Ok(())
}

fn safe_create_dir_all(root: &Path, path: &Path) -> Result<(), ManagerError> {
    if !path.starts_with(root) {
        return Err(ManagerError::Invalid("manager path"));
    }
    manager_path(root, path)?;
    // Walk from the filesystem root, rather than calling create_dir_all: a
    // fresh XDG root may have several missing ancestors, and every component
    // must be checked before it is traversed.
    let mut current = PathBuf::from("/");
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => return Err(ManagerError::Invalid("manager path component")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(ManagerError::Io(e)),
        }
    }
    Ok(())
}

pub fn write_receipt_atomic(path: &Path, receipt: &Receipt) -> Result<(), ManagerError> {
    validate_plugin_id(&receipt.id)?;
    validate_commit(&receipt.commit)?;
    let executable = receipt
        .executable
        .to_str()
        .ok_or(ManagerError::Invalid("receipt path"))?;
    if let Some(digest) = &receipt.digest {
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ManagerError::Invalid("receipt digest"));
        }
    }
    let data = format!(
        "format=teddy-receipt.v1\nid={}\ncommit={}\nexecutable={}\n{}",
        receipt.id,
        receipt.commit,
        executable,
        receipt
            .digest
            .as_ref()
            .map(|d| format!("digest={d}\n"))
            .unwrap_or_default()
    );
    atomic_write(path, data.as_bytes())
}
pub fn read_receipt(path: &Path) -> Result<Receipt, ManagerError> {
    reject_symlink_components(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(ManagerError::Invalid("receipt symlink"));
    }
    let fields = parse_fields(&read_bounded(path)?, "teddy-receipt.v1")?;
    let digest = fields.get("digest").cloned();
    if let Some(d) = &digest {
        if d.len() != 64 || !d.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ManagerError::Invalid("receipt digest"));
        }
    }
    Ok(Receipt {
        id: required(&fields, "id")?.to_owned(),
        commit: required(&fields, "commit")?.to_owned(),
        executable: validate_relative_executable(required(&fields, "executable")?)?,
        digest,
    })
}

pub fn payload_digest(path: &Path) -> Result<String, ManagerError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > 64 * 1024 * 1024 {
            return Err(ManagerError::Limit("payload"));
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}
pub fn write_state_atomic(path: &Path, records: &[StateRecord]) -> Result<(), ManagerError> {
    reject_active_update_sibling(path)?;
    if records.len() > MAX_RECORDS {
        return Err(ManagerError::Limit("state records"));
    }
    let mut data = String::from("format=teddy-state.v1\n");
    for r in records {
        validate_plugin_id(&r.id)?;
        validate_commit(&r.commit)?;
        data.push_str(&format!(
            "record={}\t{}\t{}\n",
            r.id,
            r.commit,
            u8::from(r.enabled)
        ));
    }
    atomic_write(path, data.as_bytes())
}

pub fn write_journal_atomic(path: &Path, record: &JournalRecord) -> Result<(), ManagerError> {
    reject_active_update_sibling(path)?;
    validate_plugin_id(&record.id)?;
    validate_commit(&record.commit)?;
    if record.operation.len() > 32
        || record.phase.len() > 32
        || !record
            .operation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || !record
            .phase
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(ManagerError::Invalid("journal record"));
    }
    atomic_write(
        path,
        format!(
            "format=teddy-journal.v1\noperation={}\nid={}\ncommit={}\nphase={}\n",
            record.operation, record.id, record.commit, record.phase
        )
        .as_bytes(),
    )
}

fn reject_active_update_sibling(path: &Path) -> Result<(), ManagerError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let update = parent.join("update-journal");
    if update.exists() {
        return Err(ManagerError::Recovery("update transaction is active"));
    }
    Ok(())
}

pub fn read_journal(path: &Path) -> Result<JournalRecord, ManagerError> {
    let fields = parse_fields(&read_bounded(path)?, "teddy-journal.v1")?;
    let operation = required(&fields, "operation")?.to_owned();
    let phase = required(&fields, "phase")?.to_owned();
    let record = JournalRecord {
        operation,
        id: required(&fields, "id")?.to_owned(),
        commit: required(&fields, "commit")?.to_owned(),
        phase,
    };
    write_journal_validation(&record)?;
    Ok(record)
}

fn write_journal_validation(record: &JournalRecord) -> Result<(), ManagerError> {
    validate_plugin_id(&record.id)?;
    validate_commit(&record.commit)?;
    let valid = match record.operation.as_str() {
        "install" => matches!(record.phase.as_str(), "staging" | "promotion" | "trash"),
        "remove" => matches!(
            record.phase.as_str(),
            "remove-intent" | "remove-trash" | "remove-delete"
        ),
        _ => false,
    };
    if !valid {
        return Err(ManagerError::Invalid("journal state"));
    }
    Ok(())
}

pub fn recover_manager(paths: &ManagerPaths) -> Result<(), ManagerError> {
    safe_create_dir_all(&paths.data, &paths.data)?;
    safe_create_dir_all(&paths.state, &paths.state)?;
    let journal = paths.state.join("journal");
    manager_path(&paths.state, &journal)?;
    let update_journal = paths.state.join("update-journal");
    manager_path(&paths.state, &update_journal)?;
    if update_journal.exists() {
        // An active update owns both immutable versions. Validate and retain
        // it for the UI; no generic artifact cleanup may run first.
        read_update_journal(paths)?;
        return Ok(());
    }
    if !journal.exists() {
        recover_stale(paths)?;
        return Ok(());
    }
    let record = read_journal(&journal)?;
    let stage = paths
        .data
        .join(format!(".staging-{}-{}", record.id, record.commit));
    let trash = paths
        .data
        .join(format!(".trash-{}-{}", record.id, record.commit));
    manager_path(&paths.data, &stage)?;
    manager_path(&paths.data, &trash)?;
    if record.operation == "remove" {
        let source_parent = paths.data.join(&record.id);
        manager_path(&paths.data, &source_parent)?;
        if source_parent.exists() {
            sync_dir(&source_parent)?;
        }
        sync_dir(&paths.data)?;
        match record.phase.as_str() {
            "remove-intent" | "remove-trash" | "remove-delete" => {
                if trash.exists() {
                    safe_remove_dir(&paths.data, &trash)?;
                }
            }
            _ => return Err(ManagerError::Invalid("journal state")),
        }
        remove_journal(&journal)?;
        // Only after active removal recovery is durable may generic orphan
        // cleanup run. In particular, it cannot consume this trash first.
        recover_stale(paths)?;
        return Ok(());
    }
    // Active install artifacts are removed by the journaled path first; the
    // nonce is intentionally not stored in the bounded journal, so enumerate
    // only validated staging names for this exact id/commit.
    remove_staging_for(paths, &record.id, &record.commit)?;
    match record.phase.as_str() {
        "staging" => {
            if stage.exists() {
                safe_remove_dir(&paths.data, &stage)?;
            }
        }
        "promotion" => {
            if stage.exists() {
                safe_remove_dir(&paths.data, &stage)?;
            }
            if trash.exists() {
                safe_remove_dir(&paths.data, &trash)?;
            }
        }
        "trash" => {
            if trash.exists() {
                safe_remove_dir(&paths.data, &trash)?;
            }
        }
        _ => return Err(ManagerError::Invalid("journal state")),
    }
    remove_journal(&journal)?;
    recover_stale(paths)?;
    Ok(())
}

fn remove_staging_for(paths: &ManagerPaths, id: &str, commit: &str) -> Result<(), ManagerError> {
    if !paths.data.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&paths.data)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((found_id, found_commit, is_staging)) = parse_manager_artifact_name(&name) else {
            continue;
        };
        if is_staging && found_id == id && found_commit == commit && entry.file_type()?.is_dir() {
            safe_remove_dir(&paths.data, &entry.path())?;
        }
    }
    Ok(())
}

fn safe_remove_dir(root: &Path, path: &Path) -> Result<(), ManagerError> {
    manager_path(root, path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(ManagerError::Invalid("manager artifact"));
    }
    fs::remove_dir_all(path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
        if let Some(grandparent) = parent.parent() {
            sync_dir(grandparent)?;
        }
    }
    Ok(())
}

fn remove_journal(path: &Path) -> Result<(), ManagerError> {
    fs::remove_file(path).map_err(|_| ManagerError::Recovery("journal removal failed"))?;
    sync_dir(
        path.parent()
            .ok_or(ManagerError::Recovery("journal parent"))?,
    )
}

pub struct ManagerLock {
    file: File,
}
impl ManagerLock {
    pub fn acquire(path: &Path) -> Result<Self, ManagerError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { file }),
            Err(e) => {
                let io = io::Error::from(e);
                if io.kind() == io::ErrorKind::WouldBlock {
                    Err(ManagerError::LockBusy)
                } else {
                    Err(ManagerError::Io(io))
                }
            }
        }
    }

    pub fn acquire_for(paths: &ManagerPaths) -> Result<Self, ManagerError> {
        safe_create_dir_all(&paths.state, &paths.state)?;
        Self::acquire(&paths.state.join("lock"))
    }
}

fn valid_github_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}
fn required<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, ManagerError> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or(ManagerError::Missing("field"))
}
fn make_descriptor(fields: &BTreeMap<String, String>) -> Result<PluginManifest, ManagerError> {
    validate_plugin_id(required(fields, "id")?)?;
    let repository = canonical_github_url(required(fields, "repo")?)?;
    let commit = required(fields, "commit")?.to_owned();
    validate_commit(&commit)?;
    let mut executables = BTreeMap::new();
    for (key, value) in fields {
        if let Some(target) = key.strip_prefix("executable.") {
            if target.is_empty()
                || executables
                    .insert(target.to_owned(), validate_relative_executable(value)?)
                    .is_some()
            {
                return Err(ManagerError::Duplicate("executable"));
            }
        }
    }
    if executables.is_empty() {
        return Err(ManagerError::Missing("executable"));
    }
    Ok(PluginManifest {
        id: required(fields, "id")?.to_owned(),
        repository,
        commit,
        executables,
    })
}
fn parse_fields(bytes: &[u8], expected: &str) -> Result<BTreeMap<String, String>, ManagerError> {
    if bytes.len() > MAX_BYTES {
        return Err(ManagerError::Limit("manifest"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ManagerError::Invalid("UTF-8"))?;
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or(ManagerError::Invalid("field"))?;
        let key = k.trim();
        let value = v.trim();
        if key.is_empty() || value.is_empty() {
            return Err(ManagerError::Invalid("field"));
        }
        if fields.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(ManagerError::Duplicate("field"));
        }
    }
    if required(&fields, "format")? != expected {
        return Err(ManagerError::Invalid("format"));
    }
    for key in fields.keys() {
        let allowed = if expected == "teddy-receipt.v1" {
            matches!(
                key.as_str(),
                "format" | "id" | "commit" | "executable" | "digest"
            )
        } else if expected == "teddy-journal.v1" {
            matches!(
                key.as_str(),
                "format" | "operation" | "id" | "commit" | "phase"
            )
        } else if expected == "teddy-update.v1" {
            matches!(
                key.as_str(),
                "format"
                    | "id"
                    | "stage"
                    | "old.commit"
                    | "old.payload"
                    | "old.digest"
                    | "candidate.commit"
                    | "candidate.payload"
                    | "candidate.digest"
            )
        } else {
            matches!(key.as_str(), "format" | "id" | "repo" | "commit")
                || key.starts_with("executable.")
        };
        if !allowed {
            return Err(ManagerError::Unknown("field"));
        }
    }
    Ok(fields)
}
fn split_blocks(bytes: &[u8]) -> Result<Vec<&[u8]>, ManagerError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ManagerError::Invalid("catalog UTF-8"))?;
    let mut out = Vec::new();
    let mut start = 0;
    for (i, line) in text.match_indices("\n\n") {
        out.push(&bytes[start..i]);
        start = i + line.len();
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    Ok(out)
}
fn read_bounded(path: &Path) -> Result<Vec<u8>, ManagerError> {
    let mut f = File::open(path)?;
    let mut b = Vec::new();
    f.take((MAX_BYTES + 1) as u64).read_to_end(&mut b)?;
    if b.len() > MAX_BYTES {
        Err(ManagerError::Limit("file"))
    } else {
        Ok(b)
    }
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ManagerError> {
    if bytes.len() > MAX_BYTES {
        return Err(ManagerError::Limit("file"));
    }
    let parent = path
        .parent()
        .ok_or(ManagerError::Invalid("persistence path"))?;
    reject_symlink_components(parent)?;
    if !parent.exists() {
        safe_create_dir_all(Path::new("/"), parent)?;
    }
    let target_meta = fs::symlink_metadata(path);
    if let Ok(meta) = target_meta {
        if meta.file_type().is_symlink() {
            return Err(ManagerError::Invalid("persistence symlink"));
        }
    }
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.tmp-{n}",
        path.file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("manager")
    ));
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    sync_dir(parent)?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<(), ManagerError> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|_| ManagerError::Recovery("durability sync failed"))
}

fn reject_symlink_components(path: &Path) -> Result<(), ManagerError> {
    if !path.is_absolute() {
        return Err(ManagerError::Invalid("persistence path"));
    }
    let mut current = PathBuf::from("/");
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if let Ok(meta) = fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(ManagerError::Invalid("persistence symlink"));
            }
        }
    }
    Ok(())
}

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    used: usize,
    length: u64,
}
impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            used: 0,
            length: 0,
        }
    }
    fn update(&mut self, mut input: &[u8]) {
        self.length = self.length.saturating_add(input.len() as u64);
        while !input.is_empty() {
            let take = (64 - self.used).min(input.len());
            self.block[self.used..self.used + take].copy_from_slice(&input[..take]);
            self.used += take;
            input = &input[take..];
            if self.used == 64 {
                self.compress();
                self.used = 0;
            }
        }
    }
    fn compress(&mut self) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(self.block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let a = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let b = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(a)
                .wrapping_add(w[i - 7])
                .wrapping_add(b);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            self.state[0],
            self.state[1],
            self.state[2],
            self.state[3],
            self.state[4],
            self.state[5],
            self.state[6],
            self.state[7],
        );
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }
    fn finish(mut self) -> String {
        let bits = self.length * 8;
        self.update(&[0x80]);
        while self.used != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        self.state.iter().map(|x| format!("{x:08x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn commit() -> &'static str {
        "0123456789abcdef0123456789abcdef01234567"
    }
    #[test]
    fn paths_support_overrides_and_fallbacks() {
        let p = ManagerPaths::from_values(None, None, None, Some(PathBuf::from("/h"))).unwrap();
        assert_eq!(p.config, PathBuf::from("/h/.config/teddy/plugin-manager"));
        let p = ManagerPaths::from_values(
            Some(PathBuf::from("/c")),
            Some(PathBuf::from("/d")),
            Some(PathBuf::from("/s")),
            Some(PathBuf::from("/h")),
        )
        .unwrap();
        assert_eq!(p.data, PathBuf::from("/d/teddy/plugins"));
    }
    #[test]
    fn parser_rejects_bad_corpus() {
        let good = format!("format=teddy-plugin.v1\nid=alpha\nrepo=https://github.com/a/b.git\ncommit={}\nexecutable.x=bin/x\n", commit());
        assert!(parse_manifest(good.as_bytes()).is_ok());
        assert!(parse_manifest(good.replace("id=alpha", "id=../x").as_bytes()).is_err());
        assert!(parse_manifest(good.replace("commit=", "commit=bad").as_bytes()).is_err());
        assert!(parse_manifest(format!("{}id=alpha\n", good).as_bytes()).is_err());
        assert!(canonical_github_url("https://github.com/a/b").is_err());
        assert!(validate_relative_executable("../x").is_err());
    }

    #[test]
    fn catalog_rejects_duplicate_plugin_ids() {
        let block = format!(
            "format=teddy-catalog.v1\nid=alpha\nrepo=https://github.com/a/b.git\ncommit={}\nexecutable.x=payload\n",
            commit()
        );
        let duplicate = format!("{}\n{}", block, block);
        assert!(matches!(
            parse_catalog(duplicate.as_bytes()),
            Err(ManagerError::Duplicate(_))
        ));
    }
    #[test]
    fn paths_are_validated() {
        let p = ManagerPaths::from_values(
            None,
            Some(PathBuf::from("/private/tmp/data")),
            None,
            Some(PathBuf::from("/h")),
        )
        .unwrap();
        assert!(payload_path(&p, "alpha", commit())
            .unwrap()
            .starts_with(&p.data));
        assert!(payload_path(&p, "teddy.x", commit()).is_err());
        assert!(payload_path(&p, "alpha", "ABC").is_err());
        assert!(select_executable(
            &[("x".into(), "bin/x".into()), ("x".into(), "bin/y".into())],
            "x"
        )
        .is_err());
    }
    #[test]
    fn receipt_round_trip_and_lock_contention() {
        let root =
            PathBuf::from("/private/tmp").join(format!("teddy-manager-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let rpath = root.join("receipt");
        let r = Receipt {
            id: "alpha".into(),
            commit: commit().into(),
            executable: PathBuf::from("bin/x"),
            digest: None,
        };
        write_receipt_atomic(&rpath, &r).unwrap();
        assert_eq!(read_receipt(&rpath).unwrap(), r);
        let lpath = root.join("lock");
        let a = ManagerLock::acquire(&lpath).unwrap();
        assert!(matches!(
            ManagerLock::acquire(&lpath),
            Err(ManagerError::LockBusy)
        ));
        drop(a);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn git_inputs_and_tree_are_strict() {
        assert_eq!(
            canonical_github_url("https://github.com/Acme/tool.git").unwrap(),
            "https://github.com/Acme/tool.git"
        );
        for url in [
            "http://github.com/a/b.git",
            "https://github.com/a/b.git?x",
            "git@github.com:a/b.git",
            "https://u@github.com/a/b.git",
            "https://github.com/a/b.git/",
        ] {
            assert!(canonical_github_url(url).is_err());
        }
        assert!(validate_commit(&"A".repeat(40)).is_err());
        let good = b"100755 blob 0123456789abcdef0123456789abcdef01234567\tbin/x\0";
        assert_eq!(
            parse_tree_records(good).unwrap()[0].path,
            PathBuf::from("bin/x")
        );
        for tree in [
            b"120000 blob 0123456789abcdef0123456789abcdef01234567\tx\0".as_slice(),
            b"160000 commit 0123456789abcdef0123456789abcdef01234567\tx\0",
            b"100644 blob 0123456789abcdef0123456789abcdef01234567\t../x\0",
            b"100644 blob 0123456789abcdef0123456789abcdef01234567\t.git/x\0",
        ] {
            assert!(parse_tree_records(tree).is_err());
        }
    }

    #[test]
    fn git_resolution_and_environment_are_safe() {
        let root = std::env::temp_dir().join(format!(
            "teddy-git-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let git = root.join("git");
        File::create(&git).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let found = resolve_git_values(Some(git.clone()), None).unwrap();
        let c = git_command(&found, &["init", "--"]);
        assert_eq!(c.get_program(), git.as_os_str());
        assert!(resolve_git_values(
            Some(root.join("missing")),
            Some(vec![PathBuf::from("relative")])
        )
        .is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn payload_and_stale_recovery_are_conservative() {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-recover-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let data = root.join("data");
        fs::create_dir_all(data.join(".staging-alpha-0123456789abcdef0123456789abcdef01234567-1"))
            .unwrap();
        fs::create_dir_all(data.join(format!(".trash-alpha-{}", commit()))).unwrap();
        fs::create_dir_all(data.join("keep-me")).unwrap();
        fs::create_dir_all(data.join(".trash-not-a-manager-artifact")).unwrap();
        let paths = ManagerPaths {
            config: root.join("c"),
            data: data.clone(),
            state: root.join("s"),
        };
        recover_stale(&paths).unwrap();
        assert!(!data
            .join(".staging-alpha-0123456789abcdef0123456789abcdef01234567-1")
            .exists());
        assert!(!data.join(format!(".trash-alpha-{}", commit())).exists());
        assert!(data.join("keep-me").exists());
        assert!(data.join(".trash-not-a-manager-artifact").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn digest_and_journal_are_deterministic_and_bounded() {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-journal-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let payload = root.join("payload");
        fs::write(&payload, b"abc").unwrap();
        assert_eq!(
            payload_digest(&payload).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let journal = root.join("journal");
        let record = JournalRecord {
            operation: "install".into(),
            id: "alpha".into(),
            commit: commit().into(),
            phase: "staging".into(),
        };
        write_journal_atomic(&journal, &record).unwrap();
        assert_eq!(read_journal(&journal).unwrap().phase, "staging");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn manager_paths_reject_symlink_components() {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-link-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("real");
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(unix)]
        {
            let paths = ManagerPaths {
                config: root.join("c"),
                data: link,
                state: root.join("s"),
            };
            assert!(payload_path(&paths, "alpha", commit()).is_err());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_removal_trash_is_not_orphan_cleaned_before_source_validation() {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-active-trash-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let paths = ManagerPaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        fs::create_dir_all(&paths.data).unwrap();
        fs::create_dir_all(&paths.state).unwrap();
        let trash = trash_path(&paths, "alpha", commit()).unwrap();
        fs::create_dir_all(&trash).unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, paths.data.join("alpha")).unwrap();
        write_journal_atomic(
            &paths.state.join("journal"),
            &JournalRecord {
                operation: "remove".into(),
                id: "alpha".into(),
                commit: commit().into(),
                phase: "remove-trash".into(),
            },
        )
        .unwrap();
        assert!(recover_manager(&paths).is_err());
        assert!(trash.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_transaction_journals_stages_and_retains_both_versions() {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-update-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let paths = ManagerPaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let candidate_commit = "f".repeat(40);
        let mut identities = Vec::new();
        for (commit_value, bytes) in [
            (commit().to_owned(), b"old".as_slice()),
            (candidate_commit, b"candidate".as_slice()),
        ] {
            let version = paths.data.join("alpha").join(&commit_value);
            fs::create_dir_all(&version).unwrap();
            let payload = version.join("payload");
            fs::write(&payload, bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&payload, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let digest = payload_digest(&payload).unwrap();
            let receipt = Receipt {
                id: "alpha".into(),
                commit: commit_value,
                executable: PathBuf::from("payload"),
                digest: Some(digest),
            };
            write_receipt_atomic(&version.join("receipt"), &receipt).unwrap();
            identities.push(UpdateIdentity { receipt, payload });
        }
        let lock = ManagerLock::acquire_for(&paths).unwrap();
        let transaction =
            begin_update(&paths, &lock, identities[0].clone(), identities[1].clone()).unwrap();
        assert_eq!(pending_update(&paths).unwrap(), Some(transaction.clone()));
        assert!(matches!(
            begin_update(&paths, &lock, identities[0].clone(), identities[1].clone()),
            Err(ManagerError::Duplicate(_))
        ));
        let transaction =
            advance_update(&paths, &lock, &transaction, UpdateStage::OldQuiesced).unwrap();
        assert_eq!(
            pending_update(&paths).unwrap().unwrap().stage,
            UpdateStage::OldQuiesced
        );
        assert!(advance_update(&paths, &lock, &transaction, UpdateStage::RollbackRunning).is_err());
        let transaction = advance_update(
            &paths,
            &lock,
            &transaction,
            UpdateStage::CandidateEnableRequested,
        )
        .unwrap();
        let transaction =
            advance_update(&paths, &lock, &transaction, UpdateStage::CandidateRunning).unwrap();
        let _ = advance_update(&paths, &lock, &transaction, UpdateStage::Completed).unwrap();
        assert!(pending_update(&paths).unwrap().is_none());
        assert!(identities[0].payload.exists());
        assert!(identities[1].payload.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_update_blocks_ordinary_removal_and_state_mutation() {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-update-guard-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let paths = ManagerPaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let candidate_commit = "e".repeat(40);
        let mut identities = Vec::new();
        for commit_value in [commit().to_owned(), candidate_commit] {
            let version = paths.data.join("alpha").join(&commit_value);
            fs::create_dir_all(&version).unwrap();
            let payload = version.join("payload");
            fs::write(&payload, b"verified").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&payload, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let receipt = Receipt {
                id: "alpha".into(),
                commit: commit_value,
                executable: PathBuf::from("payload"),
                digest: Some(payload_digest(&payload).unwrap()),
            };
            write_receipt_atomic(&version.join("receipt"), &receipt).unwrap();
            identities.push(UpdateIdentity { receipt, payload });
        }
        let lock = ManagerLock::acquire_for(&paths).unwrap();
        begin_update(&paths, &lock, identities[0].clone(), identities[1].clone()).unwrap();
        for identity in &identities {
            assert!(matches!(
                remove_installed(&paths, &lock, &identity.receipt),
                Err(ManagerError::Recovery(_))
            ));
            assert!(identity.payload.exists());
        }
        assert!(matches!(
            write_state_atomic(&paths.state.join("state"), &[]),
            Err(ManagerError::Recovery(_))
        ));
        assert!(pending_update(&paths).unwrap().is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_update_journal_is_refused_without_cleanup() {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-update-bad-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let paths = ManagerPaths {
            config: root.join("c"),
            data: root.join("data"),
            state: root.join("state"),
        };
        fs::create_dir_all(&paths.data).unwrap();
        fs::create_dir_all(&paths.state).unwrap();
        let stale = paths
            .data
            .join(".staging-alpha-0123456789abcdef0123456789abcdef01234567-1");
        fs::create_dir_all(&stale).unwrap();
        fs::write(
            paths.state.join("update-journal"),
            b"format=teddy-update.v1\nid=bad\n",
        )
        .unwrap();
        assert!(recover_manager(&paths).is_err());
        assert!(stale.exists());
        let _ = fs::remove_dir_all(root);
    }

    fn removal_fixture(tag: &str) -> (PathBuf, ManagerPaths, Receipt) {
        let root = PathBuf::from("/private/tmp").join(format!(
            "teddy-remove-{tag}-{}",
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let paths = ManagerPaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let version = paths.data.join("alpha").join(commit());
        fs::create_dir_all(&version).unwrap();
        let payload = version.join("payload");
        fs::write(&payload, b"safe payload").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&payload, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let digest = payload_digest(&payload).unwrap();
        let receipt = Receipt {
            id: "alpha".into(),
            commit: commit().into(),
            executable: PathBuf::from("payload"),
            digest: Some(digest),
        };
        write_receipt_atomic(&version.join("receipt"), &receipt).unwrap();
        (root, paths, receipt)
    }

    #[test]
    fn journaled_removal_is_verified_and_idempotent() {
        let (root, paths, receipt) = removal_fixture("ok");
        let lock = ManagerLock::acquire_for(&paths).unwrap();
        assert_eq!(
            remove_installed(&paths, &lock, &receipt).unwrap(),
            RemovalResult::Removed
        );
        assert_eq!(
            remove_installed(&paths, &lock, &receipt).unwrap(),
            RemovalResult::NotFound
        );
        assert!(!paths.data.join("alpha").join(commit()).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_refuses_stale_receipts_and_recovers_trash() {
        let (root, paths, mut receipt) = removal_fixture("stale");
        receipt.digest = Some("00".repeat(32));
        let lock = ManagerLock::acquire_for(&paths).unwrap();
        assert!(matches!(
            remove_installed(&paths, &lock, &receipt),
            Err(ManagerError::Invalid(_))
        ));
        let trash = trash_path(&paths, "alpha", commit()).unwrap();
        let version = paths.data.join("alpha").join(commit());
        fs::rename(&version, &trash).unwrap();
        write_journal_atomic(
            &paths.state.join("journal"),
            &JournalRecord {
                operation: "remove".into(),
                id: "alpha".into(),
                commit: commit().into(),
                phase: "remove-trash".into(),
            },
        )
        .unwrap();
        recover_manager(&paths).unwrap();
        assert!(!trash.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removal_rejects_symlink_payload_and_escape() {
        let (root, paths, receipt) = removal_fixture("link");
        let payload = paths.data.join("alpha").join(commit()).join("payload");
        let outside = root.join("outside");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&payload).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &payload).unwrap();
        let lock = ManagerLock::acquire_for(&paths).unwrap();
        assert!(remove_installed(&paths, &lock, &receipt).is_err());
        let _ = fs::remove_dir_all(root);
    }
}
