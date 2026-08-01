//! Durable configuration for externally hosted plugins.
//!
//! This module deliberately does not inspect or execute plugin paths.  The
//! host lane can use [`Registry::load`], then use `list`, `get`, `upsert`,
//! `remove`, and `toggle` before calling [`Registry::save`].

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 4] = b"TDRG";
const VERSION: u16 = 2;
const MAX_RECORDS: u32 = 1024;
const MAX_ID: usize = 64;
const MAX_PATH: usize = 16 * 1024;
const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_RESTARTS: u32 = 3;
const MAX_BACKOFF_MS: u32 = 86_400_000;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Stable, validated plugin identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(String);

impl PluginId {
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_ID
            || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
            || !bytes.iter().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            })
        {
            return Err(RegistryError::InvalidId(value));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Restart limits are bounded so malformed configuration cannot cause an
/// unbounded supervisor loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub backoff_ms: u32,
}

impl RestartPolicy {
    pub fn new(max_restarts: u32, backoff_ms: u32) -> Result<Self, RegistryError> {
        if max_restarts > MAX_RESTARTS || backoff_ms > MAX_BACKOFF_MS {
            return Err(RegistryError::Limit("restart policy"));
        }
        Ok(Self {
            max_restarts,
            backoff_ms,
        })
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            backoff_ms: 1000,
        }
    }
}

/// A configured executable. `path` is absolute and is retained byte-for-byte
/// on Unix, including non-UTF-8 names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRecord {
    pub id: PluginId,
    pub path: PathBuf,
    pub enabled: bool,
    pub restart: RestartPolicy,
    pub generation: u64,
}

#[derive(Debug)]
pub enum RegistryError {
    Io(io::Error),
    Corrupt(&'static str),
    UnknownVersion(u16),
    Conflict,
    InvalidId(String),
    RelativePath,
    InvalidPathEncoding,
    DuplicateId(PluginId),
    Limit(&'static str),
    Busy,
    NotFound(PluginId),
}

/// Result of a successful registry commit.  `CommittedWithWarning` means the
/// rename is visible and the in-memory generation is current, but the parent
/// directory could not be synced and durability across sudden power loss is
/// not guaranteed.
#[derive(Debug)]
pub enum SaveOutcome {
    Durable,
    CommittedWithWarning { error: io::Error },
}
impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "registry I/O: {e}"),
            Self::Corrupt(s) => write!(f, "corrupt registry ({s})"),
            Self::UnknownVersion(v) => write!(f, "unknown registry version {v}"),
            Self::Conflict => write!(f, "plugin registry changed on disk"),
            Self::InvalidId(s) => write!(f, "invalid plugin id {s:?}"),
            Self::RelativePath => write!(f, "plugin path is not absolute"),
            Self::InvalidPathEncoding => write!(f, "invalid plugin path encoding"),
            Self::DuplicateId(id) => write!(f, "duplicate plugin id {id}"),
            Self::Limit(s) => write!(f, "registry {s} exceeds limit"),
            Self::Busy => write!(f, "plugin registry is busy"),
            Self::NotFound(id) => write!(f, "plugin not found: {id}"),
        }
    }
}
impl std::error::Error for RegistryError {}
impl From<io::Error> for RegistryError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// In-memory registry. Mutations assign strictly increasing generations.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    records: Vec<PluginRecord>,
    next_generation: u64,
    base_generation: u64,
    path: Option<PathBuf>,
    recovery_required: bool,
}

impl Registry {
    /// Load the XDG configuration file; a missing file is an empty registry.
    pub fn load() -> Result<Self, RegistryError> {
        Self::load_at(&default_path()?)
    }
    /// Load from an explicit path, useful for tests and alternate profiles.
    pub fn load_at(path: &Path) -> Result<Self, RegistryError> {
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: Some(path.to_owned()),
                    ..Self::default()
                })
            }
            Err(e) => return Err(e.into()),
        };
        if !metadata.is_file() {
            return Err(RegistryError::Corrupt("registry is not a regular file"));
        }
        let len = metadata.len();
        if len > MAX_FILE {
            return Err(RegistryError::Limit("file"));
        }
        let mut file = File::open(path)?;
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes)?;
        let mut r = Decoder::new(&bytes);
        if r.take(4)? != MAGIC {
            return Err(RegistryError::Corrupt("magic"));
        }
        let version = r.u16()?;
        if version != VERSION {
            return Err(RegistryError::UnknownVersion(version));
        }
        let generation = r.u64()?;
        let count = r.u32()?;
        if count > MAX_RECORDS {
            return Err(RegistryError::Limit("record count"));
        }
        let mut out = Self {
            base_generation: generation,
            next_generation: generation,
            path: Some(path.to_owned()),
            ..Self::default()
        };
        for _ in 0..count {
            let id = PluginId::new(r.string(MAX_ID)?)?;
            let path = decode_path(r.bytes(MAX_PATH)?)?;
            if !path.is_absolute() {
                return Err(RegistryError::RelativePath);
            }
            let restart = RestartPolicy::new(r.u32()?, r.u32()?)?;
            let record = PluginRecord {
                id: id.clone(),
                path,
                enabled: r.byte()? != 0,
                restart,
                generation: r.u64()?,
            };
            if out.records.iter().any(|x| x.id == id) {
                return Err(RegistryError::DuplicateId(id));
            }
            out.next_generation = out.next_generation.max(record.generation);
            out.records.push(record);
        }
        if r.remaining() != 0 {
            return Err(RegistryError::Corrupt("trailing data"));
        }
        Ok(out)
    }
    /// Load while preserving a usable empty registry on damaged input.  The
    /// error is returned separately so callers can report it and continue.
    pub fn load_recoverable_at(path: &Path) -> (Self, Option<RegistryError>) {
        match Self::load_at(path) {
            Ok(registry) => (registry, None),
            Err(error) => (
                Self {
                    recovery_required: true,
                    path: Some(path.to_owned()),
                    ..Self::default()
                },
                Some(error),
            ),
        }
    }
    pub fn list(&self) -> &[PluginRecord] {
        &self.records
    }
    pub fn get(&self, id: &PluginId) -> Option<&PluginRecord> {
        self.records.iter().find(|r| &r.id == id)
    }
    pub fn upsert(&mut self, mut record: PluginRecord) -> Result<(), RegistryError> {
        validate_record(&record)?;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(RegistryError::Limit("generation"))?;
        record.generation = self.next_generation;
        if let Some(old) = self.records.iter_mut().find(|r| r.id == record.id) {
            *old = record;
        } else {
            if self.records.len() == MAX_RECORDS as usize {
                return Err(RegistryError::Limit("record count"));
            }
            self.records.push(record);
        }
        Ok(())
    }
    pub fn remove(&mut self, id: &PluginId) -> Result<PluginRecord, RegistryError> {
        self.records
            .iter()
            .position(|r| &r.id == id)
            .map(|i| self.records.remove(i))
            .ok_or_else(|| RegistryError::NotFound(id.clone()))
    }
    pub fn toggle(&mut self, id: &PluginId) -> Result<bool, RegistryError> {
        let mut r = self
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(id.clone()))?;
        r.enabled = !r.enabled;
        let enabled = r.enabled;
        self.upsert(r)?;
        Ok(enabled)
    }
    /// Persist using the path used by load, or the XDG default for a new registry.
    pub fn save(&mut self) -> Result<SaveOutcome, RegistryError> {
        let path = self.path.clone().unwrap_or(default_path()?);
        self.save_at(&path)
    }
    pub fn save_at(&mut self, path: &Path) -> Result<SaveOutcome, RegistryError> {
        self.save_at_inner(path, false)
    }
    /// Explicitly replace damaged or unknown state with this registry.
    pub fn save_recovered_at(&mut self, path: &Path) -> Result<SaveOutcome, RegistryError> {
        self.save_at_inner(path, true)
    }
    fn save_at_inner(
        &mut self,
        path: &Path,
        explicit_recovery: bool,
    ) -> Result<SaveOutcome, RegistryError> {
        self.save_at_inner_with_sync(path, explicit_recovery, sync_dir)
    }
    fn save_at_inner_with_sync<F>(
        &mut self,
        path: &Path,
        explicit_recovery: bool,
        sync: F,
    ) -> Result<SaveOutcome, RegistryError>
    where
        F: FnOnce(&Path) -> io::Result<()>,
    {
        if self.recovery_required && !explicit_recovery {
            return Err(RegistryError::Corrupt("recovery required before save"));
        }
        if self.records.len() > MAX_RECORDS as usize {
            return Err(RegistryError::Limit("record count"));
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let _lock = Lock::acquire(&path.with_extension("lock"))?;
        if !explicit_recovery {
            let disk_generation = disk_generation(path)?;
            if disk_generation != self.base_generation {
                return Err(RegistryError::Conflict);
            }
        }
        let generation = self
            .base_generation
            .checked_add(1)
            .ok_or(RegistryError::Limit("generation"))?;
        let mut data = Vec::new();
        data.extend_from_slice(MAGIC);
        put_u16(&mut data, VERSION);
        put_u64(&mut data, generation);
        put_u32(&mut data, self.records.len() as u32);
        for r in &self.records {
            validate_record(r)?;
            put_bytes(&mut data, r.id.as_str().as_bytes());
            put_bytes(&mut data, &encode_path(&r.path)?);
            put_u32(&mut data, r.restart.max_restarts);
            put_u32(&mut data, r.restart.backoff_ms);
            data.push(r.enabled as u8);
            put_u64(&mut data, r.generation);
        }
        if data.len() as u64 > MAX_FILE {
            return Err(RegistryError::Limit("file"));
        }
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(
            ".{}.tmp-{}-{}",
            path.file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("registry"),
            std::process::id(),
            n
        ));
        let result = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, path)?;
            Ok::<_, io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
            return result
                .map(|_| SaveOutcome::Durable)
                .map_err(RegistryError::Io);
        }
        // Rename is the commit point.  From here on, the file generation must
        // advance even if syncing the containing directory is unsuccessful.
        self.base_generation = generation;
        self.next_generation = self.next_generation.max(generation);
        self.path = Some(path.to_owned());
        self.recovery_required = false;
        match sync(parent) {
            Ok(()) => Ok(SaveOutcome::Durable),
            Err(error) => Ok(SaveOutcome::CommittedWithWarning { error }),
        }
    }
}

fn validate_record(r: &PluginRecord) -> Result<(), RegistryError> {
    PluginId::new(r.id.0.clone())?;
    if !r.path.is_absolute() {
        return Err(RegistryError::RelativePath);
    }
    RestartPolicy::new(r.restart.max_restarts, r.restart.backoff_ms)?;
    let _ = encode_path(&r.path)?;
    Ok(())
}
fn default_path() -> Result<PathBuf, RegistryError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or(RegistryError::Limit("configuration home"))?;
    Ok(base.join("teddy").join("plugins.bin"))
}

fn disk_generation(path: &Path) -> Result<u64, RegistryError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Err(RegistryError::Corrupt("registry is not a regular file"));
    }
    if metadata.len() > MAX_FILE {
        return Err(RegistryError::Limit("file"));
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    let mut decoder = Decoder::new(&bytes);
    if decoder.take(4)? != MAGIC {
        return Err(RegistryError::Corrupt("magic"));
    }
    let version = decoder.u16()?;
    if version != VERSION {
        return Err(RegistryError::UnknownVersion(version));
    }
    decoder.u64()
}

#[cfg(unix)]
fn encode_path(p: &Path) -> Result<Vec<u8>, RegistryError> {
    use std::os::unix::ffi::OsStrExt;
    let b = p.as_os_str().as_bytes();
    if b.len() > MAX_PATH {
        Err(RegistryError::Limit("path"))
    } else {
        Ok(b.to_vec())
    }
}
#[cfg(not(unix))]
fn encode_path(p: &Path) -> Result<Vec<u8>, RegistryError> {
    let s = p.to_str().ok_or(RegistryError::InvalidPathEncoding)?;
    if s.len() > MAX_PATH {
        Err(RegistryError::Limit("path"))
    } else {
        Ok(s.as_bytes().to_vec())
    }
}
#[cfg(unix)]
fn decode_path(b: &[u8]) -> Result<PathBuf, RegistryError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(b.to_vec())))
}
#[cfg(not(unix))]
fn decode_path(b: &[u8]) -> Result<PathBuf, RegistryError> {
    String::from_utf8(b.to_vec())
        .map(PathBuf::from)
        .map_err(|_| RegistryError::InvalidPathEncoding)
}

fn put_u16(v: &mut Vec<u8>, n: u16) {
    v.extend(n.to_le_bytes());
}
fn put_u32(v: &mut Vec<u8>, n: u32) {
    v.extend(n.to_le_bytes());
}
fn put_u64(v: &mut Vec<u8>, n: u64) {
    v.extend(n.to_le_bytes());
}
fn put_bytes(v: &mut Vec<u8>, b: &[u8]) {
    put_u32(v, b.len() as u32);
    v.extend(b);
}
struct Decoder<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Decoder<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.i)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], RegistryError> {
        if self.remaining() < n {
            return Err(RegistryError::Corrupt("truncated"));
        }
        let x = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(x)
    }
    fn byte(&mut self) -> Result<u8, RegistryError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, RegistryError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, RegistryError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, RegistryError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self, max: usize) -> Result<&'a [u8], RegistryError> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(RegistryError::Limit("string"));
        }
        self.take(n)
    }
    fn string(&mut self, max: usize) -> Result<String, RegistryError> {
        String::from_utf8(self.bytes(max)?.to_vec()).map_err(|_| RegistryError::Corrupt("utf-8"))
    }
}

struct Lock {
    file: File,
}
impl Lock {
    fn acquire(path: &Path) -> Result<Self, RegistryError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        #[cfg(any(unix, target_os = "redox"))]
        {
            use rustix::fs::{flock, FlockOperation};
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => Ok(Self { file }),
                Err(e) => {
                    let e = io::Error::from(e);
                    if e.kind() == io::ErrorKind::WouldBlock {
                        Err(RegistryError::Busy)
                    } else {
                        Err(e.into())
                    }
                }
            }
        }
        #[cfg(not(any(unix, target_os = "redox")))]
        {
            let _ = file;
            Err(RegistryError::Limit("advisory locking unsupported"))
        }
    }
}
fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "teddy-registry-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn record(id: &str) -> PluginRecord {
        PluginRecord {
            id: PluginId::new(id).unwrap(),
            path: PathBuf::from("/bin/true"),
            enabled: true,
            restart: RestartPolicy::default(),
            generation: 0,
        }
    }
    #[test]
    fn validates_ids() {
        assert!(PluginId::new("ok-1.x").is_ok());
        assert!(PluginId::new(&"a".repeat(64)).is_ok());
        assert!(PluginId::new(&"a".repeat(65)).is_err());
        assert!(PluginId::new("Bad").is_err());
        assert!(PluginId::new("a/b").is_err());
    }

    #[test]
    fn restart_policy_caps_geometric_retry_count() {
        assert!(RestartPolicy::new(3, 250).is_ok());
        assert!(matches!(
            RestartPolicy::new(4, 250),
            Err(RegistryError::Limit("restart policy"))
        ));
    }
    #[test]
    fn round_trip_and_mutations() {
        let p = temp();
        let mut r = Registry::default();
        r.upsert(record("one")).unwrap();
        r.save_at(&p).unwrap();
        let mut q = Registry::load_at(&p).unwrap();
        assert_eq!(q.list().len(), 1);
        assert!(!q.toggle(&PluginId::new("one").unwrap()).unwrap());
        q.remove(&PluginId::new("one").unwrap()).unwrap();
        let _ = fs::remove_file(p);
    }
    #[test]
    fn missing_is_empty() {
        let p = temp();
        assert!(Registry::load_at(&p).unwrap().list().is_empty());
    }

    #[cfg(any(unix, target_os = "redox"))]
    #[test]
    fn lock_contention_is_busy_and_recovers_after_drop() {
        let p = temp();
        let lock_path = p.with_extension("lock");
        let held = Lock::acquire(&lock_path).unwrap();
        assert!(matches!(
            Lock::acquire(&lock_path),
            Err(RegistryError::Busy)
        ));
        drop(held);
        assert!(Lock::acquire(&lock_path).is_ok());
        let _ = fs::remove_file(lock_path);
    }

    #[test]
    fn stale_instances_conflict_instead_of_overwriting() {
        let p = temp();
        let mut first = Registry::default();
        first.upsert(record("one")).unwrap();
        first.save_at(&p).unwrap();
        let mut second = Registry::load_at(&p).unwrap();
        first.toggle(&PluginId::new("one").unwrap()).unwrap();
        first.save().unwrap();
        second.toggle(&PluginId::new("one").unwrap()).unwrap();
        assert!(matches!(second.save(), Err(RegistryError::Conflict)));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn reload_mutate_save_advances_file_generation() {
        let p = temp();
        let mut first = Registry::default();
        first.upsert(record("one")).unwrap();
        first.save_at(&p).unwrap();
        let mut second = Registry::load_at(&p).unwrap();
        second.toggle(&PluginId::new("one").unwrap()).unwrap();
        second.save().unwrap();
        let third = Registry::load_at(&p).unwrap();
        assert!(!third.get(&PluginId::new("one").unwrap()).unwrap().enabled);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn encoded_size_is_checked_before_temp_creation() {
        let p = temp();
        let mut registry = Registry::default();
        let long_path = PathBuf::from(format!("/{}", "x".repeat(MAX_PATH - 1)));
        for n in 0..MAX_RECORDS {
            let mut item = record(&format!("p{n}"));
            item.path = long_path.clone();
            registry.upsert(item).unwrap();
        }
        assert!(matches!(
            registry.save_at(&p),
            Err(RegistryError::Limit("file"))
        ));
        assert!(!p.exists());
        let _ = fs::remove_file(p.with_extension("lock"));
    }

    #[test]
    fn rename_failure_removes_temporary_file() {
        let p = temp();
        fs::create_dir_all(&p).unwrap();
        let mut registry = Registry::default();
        registry.upsert(record("one")).unwrap();
        assert!(registry.save_at(&p).is_err());
        let parent = p.parent().unwrap().to_owned();
        let prefix = format!(
            ".{}.tmp-{}-",
            p.file_name().unwrap().to_string_lossy(),
            std::process::id()
        );
        assert!(!fs::read_dir(&parent).unwrap().any(|entry| {
            entry
                .ok()
                .is_some_and(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        }));
        let _ = fs::remove_dir_all(&p);
        let _ = fs::remove_file(
            parent.join(format!("{}.lock", p.file_name().unwrap().to_string_lossy())),
        );
    }

    #[test]
    fn recoverable_corruption_requires_explicit_recovery() {
        let p = temp();
        fs::write(&p, b"not a registry").unwrap();
        let (mut registry, error) = Registry::load_recoverable_at(&p);
        assert!(error.is_some());
        assert!(matches!(
            registry.save(),
            Err(RegistryError::Corrupt("recovery required before save"))
        ));
        registry.upsert(record("one")).unwrap();
        registry.save_recovered_at(&p).unwrap();
        assert_eq!(Registry::load_at(&p).unwrap().list().len(), 1);
        let _ = fs::remove_file(p);
    }

    #[test]
    fn directory_sync_warning_is_committed_and_generation_advances() {
        let p = temp();
        let mut registry = Registry::default();
        registry.upsert(record("one")).unwrap();
        let outcome = registry
            .save_at_inner_with_sync(&p, false, |_| {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected directory sync failure",
                ))
            })
            .unwrap();
        assert!(matches!(outcome, SaveOutcome::CommittedWithWarning { .. }));

        registry.toggle(&PluginId::new("one").unwrap()).unwrap();
        assert!(matches!(
            registry.save_at(&p).unwrap(),
            SaveOutcome::Durable
        ));
        let _ = fs::remove_file(p);
    }
}
