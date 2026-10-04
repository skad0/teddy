//! teddy — byte-addressed terminal editor. Single-threaded poll loop,
//! input first, paint on dirty (spec §18).

mod buffer;
mod ignore;
mod input;
#[allow(dead_code)] // wrapped into S8 plugin executables
mod lex;
mod lines;
mod picker;
mod plugin;
mod plugin_registry;
mod render;
mod search;
mod storage;
mod term;
mod watch;

use buffer::{Buffer, HUGE_WINDOW};
use input::{Key, Parser};
use render::{FrameBuf, RenderCache, View};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn registry_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("teddy").join("plugins.bin"))
}

fn durability_warning(outcome: plugin_registry::SaveOutcome) -> Option<String> {
    match outcome {
        plugin_registry::SaveOutcome::Durable => None,
        plugin_registry::SaveOutcome::CommittedWithWarning { error } => Some(error.to_string()),
    }
}

fn manager_executable(value: &std::ffi::OsStr) -> Result<PathBuf, &'static str> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("manager path must be absolute");
    }
    let metadata =
        std::fs::metadata(&path).map_err(|_| "manager path is not a regular executable")?;
    if !metadata.is_file() {
        return Err("manager path is not a regular executable");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("manager path is not executable");
        }
    }
    Ok(path)
}

struct Args {
    workspace: Option<PathBuf>,
    files: Vec<PathBuf>,
    follow: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        workspace: None,
        files: Vec::new(),
        follow: false,
    };
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
            return Err(format!(
                "{}: is a directory (use -w to open a workspace)",
                chrome_path(f)
            ));
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

    // panic path per spec §20: restore terminal (also leaves the alternate
    // screen), write the crash log, then print its path as the last line.
    // Installed before the open loop so storage panics are logged too.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term::restore();
        // ponytail: release builds are stripped, so this backtrace is addresses only
        let body = format!(
            "teddy {}\n{info}\n\nbacktrace:\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::backtrace::Backtrace::force_capture()
        );
        let written = crash_log_path(
            std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
            std::process::id(),
        )
        .ok_or_else(|| std::io::Error::other("neither XDG_STATE_HOME nor HOME is set"))
        .and_then(|path| write_crash_log(&path, &body).map(|()| path));
        default_hook(info);
        // writeln, not eprintln: a panic inside the hook would abort
        let mut err = std::io::stderr();
        let _ = match written {
            Ok(path) => writeln!(err, "teddy: crash log written to {}", chrome_path(&path)),
            Err(e) => writeln!(err, "teddy: could not write crash log: {e}"),
        };
    }));

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
                    eprintln!("teddy: {}: {e}", chrome_path(f));
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    let guard = match term::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("teddy: {e}");
            return ExitCode::FAILURE;
        }
    };

    // picker/`open` root: explicit workspace or the process cwd (spec §17)
    let root = args
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let result = run(&mut buffers, &root);
    drop(guard);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("teddy: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `$XDG_STATE_HOME/teddy/crash-<pid>.log`, falling back to `~/.local/state`.
/// Relative values are ignored (XDG requires absolute paths), so panic data
/// never lands in the launch directory.
fn crash_log_path(state_home: Option<PathBuf>, home: Option<PathBuf>, pid: u32) -> Option<PathBuf> {
    let base = state_home.filter(|p| p.is_absolute()).or_else(|| {
        home.filter(|h| h.is_absolute())
            .map(|h| h.join(".local/state"))
    })?;
    Some(base.join("teddy").join(format!("crash-{pid}.log")))
}

/// Owner-only, since a panic message may quote buffer contents.
fn write_crash_log(path: &Path, body: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    // a stale log from a reused pid is replaced, never reopened: create_new
    // guarantees 0600 and refuses to follow a symlink planted at the path
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(body.as_bytes())
}

/// Resolve the first-party plugin as an executable sibling, without shelling
/// out. Cargo builds and installed deployments use this layout.
fn bundled_highlighter_path(executable: &Path) -> PathBuf {
    executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("teddy-highlight{}", std::env::consts::EXE_SUFFIX))
}

fn configured_highlighter(paths: &[PathBuf], bundled: &Path) -> bool {
    let bundled_canonical = std::fs::canonicalize(bundled).ok();
    paths.iter().any(|path| {
        if path.file_name() == bundled.file_name() {
            return true;
        }
        match (&bundled_canonical, std::fs::canonicalize(path).ok()) {
            (Some(bundled), Some(path)) => bundled == &path,
            _ => false,
        }
    })
}

fn plugin_paths() -> Vec<(PathBuf, bool)> {
    let mut paths: Vec<PathBuf> = std::env::var_os("TEDDY_PLUGINS")
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut result: Vec<(PathBuf, bool)> = paths.drain(..).map(|path| (path, false)).collect();
    if let Ok(executable) = std::env::current_exe() {
        let bundled = bundled_highlighter_path(&executable);
        let configured = result
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        if bundled.is_file() && !configured_highlighter(&configured, &bundled) {
            result.push((bundled, true));
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LauncherAction {
    Enable,
    Disable,
    Reload,
    Forget,
}

fn dispatch_launcher_request(
    registry: &mut plugin_registry::Registry,
    request: &plugin::LauncherRequest,
    manager_path: Option<&Path>,
    recovery_required: bool,
    observed: Option<&HashMap<String, u8>>,
) -> Result<
    (
        plugin::LauncherResponse,
        Option<LauncherAction>,
        Option<String>,
    ),
    String,
> {
    let mut confirmed_recovery = false;
    if recovery_required && !matches!(request, plugin::LauncherRequest::List { .. }) {
        confirmed_recovery = matches!(
            request,
            plugin::LauncherRequest::Enable {
                descriptor: Some(plugin::LauncherDescriptor {
                    confirm_recovery: true,
                    ..
                }),
                ..
            }
        );
        if !confirmed_recovery {
            return Err("registry recovery requires explicit confirmation".into());
        }
    }
    let id = match request {
        plugin::LauncherRequest::List { page } => {
            const PAGE_SIZE: usize = 32;
            let start = (*page as usize).saturating_mul(PAGE_SIZE);
            if start > registry.list().len() {
                return Err("launcher list page out of range".into());
            }
            let end = (start + PAGE_SIZE).min(registry.list().len());
            return Ok((
                plugin::LauncherResponse::List {
                    page: *page,
                    next_page: (end < registry.list().len()).then_some(page.saturating_add(1)),
                    records: registry.list()[start..end]
                        .iter()
                        .map(|r| plugin::LauncherRecord {
                            id: r.id.to_string(),
                            path: r.path.to_string_lossy().into_owned(),
                            enabled: r.enabled,
                            state: observed
                                .and_then(|states| states.get(r.id.as_str()).copied())
                                .unwrap_or(4),
                            max_restarts: r.restart.max_restarts,
                            backoff_ms: r.restart.backoff_ms,
                        })
                        .collect(),
                },
                None,
                None,
            ));
        }
        plugin::LauncherRequest::Enable { id, .. }
        | plugin::LauncherRequest::Disable(id)
        | plugin::LauncherRequest::Reload(id)
        | plugin::LauncherRequest::Forget(id) => id,
    };
    let id = plugin_registry::PluginId::new(id.clone()).map_err(|_| "invalid plugin id")?;
    if id.as_str().starts_with("teddy.") {
        return Err("reserved plugin id".into());
    }
    if next_record_path(registry, &id).is_some_and(|path| manager_path == Some(path)) {
        return Err("manager cannot control itself".into());
    }
    let mut next = registry.clone();
    let action = match request {
        plugin::LauncherRequest::Enable { descriptor, .. } => {
            let mut record = if let Some(record) = next.get(&id).cloned() {
                if descriptor.is_some() {
                    return Err("existing plugin enable must be ID-only".into());
                }
                record
            } else {
                let descriptor = descriptor
                    .as_ref()
                    .ok_or_else(|| "unknown plugin requires descriptor".to_string())?;
                let path = validate_manager_descriptor(descriptor)?;
                plugin_registry::PluginRecord {
                    id: id.clone(),
                    path,
                    enabled: false,
                    restart: plugin_registry::RestartPolicy::new(
                        descriptor.max_restarts,
                        descriptor.backoff_ms,
                    )
                    .map_err(|e| e.to_string())?,
                    generation: 0,
                }
            };
            record.enabled = true;
            next.upsert(record).map_err(|e| e.to_string())?;
            LauncherAction::Enable
        }
        plugin::LauncherRequest::Disable(_) => {
            let mut record = next
                .get(&id)
                .cloned()
                .ok_or_else(|| "plugin is not registered".to_string())?;
            record.enabled = false;
            next.upsert(record).map_err(|e| e.to_string())?;
            LauncherAction::Disable
        }
        plugin::LauncherRequest::Reload(_) => {
            if next.get(&id).is_none() {
                return Err("plugin is not registered".into());
            }
            LauncherAction::Reload
        }
        plugin::LauncherRequest::Forget(_) => {
            if next.get(&id).is_some_and(|record| record.enabled) {
                return Err("plugin must be disabled before forget".into());
            }
            next.remove(&id).map_err(|e| e.to_string())?;
            LauncherAction::Forget
        }
        plugin::LauncherRequest::List { .. } => unreachable!(),
    };
    let save_outcome = if confirmed_recovery {
        let path = registry_config_path().ok_or_else(|| "registry path unavailable".to_string())?;
        next.save_recovered_at(&path).map_err(|e| e.to_string())?
    } else {
        next.save().map_err(|e| e.to_string())?
    };
    let warning = durability_warning(save_outcome);
    *registry = next;
    let response = match action {
        LauncherAction::Enable => plugin::LauncherResponse::Enabled(id.to_string()),
        LauncherAction::Disable => plugin::LauncherResponse::Disabled(id.to_string()),
        LauncherAction::Reload => plugin::LauncherResponse::Reloaded(id.to_string()),
        LauncherAction::Forget => plugin::LauncherResponse::Forgotten(id.to_string()),
    };
    Ok((response, Some(action), warning))
}

fn validate_manager_descriptor(descriptor: &plugin::LauncherDescriptor) -> Result<PathBuf, String> {
    if descriptor.path.len() > 16 * 1024 || !Path::new(&descriptor.path).is_absolute() {
        return Err("descriptor path must be bounded and absolute".into());
    }
    let path = PathBuf::from(&descriptor.path);
    let metadata =
        std::fs::metadata(&path).map_err(|_| "descriptor path is not a regular executable")?;
    if !metadata.is_file() {
        return Err("descriptor path is not a regular executable".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("descriptor path is not executable".into());
        }
    }
    std::fs::canonicalize(path).map_err(|_| "descriptor path cannot be canonicalized".into())
}

fn next_record_path<'a>(
    registry: &'a plugin_registry::Registry,
    id: &plugin_registry::PluginId,
) -> Option<&'a Path> {
    registry.get(id).map(|record| record.path.as_path())
}

fn spans_match(
    spans_buffer: usize,
    spans_revision: u64,
    spans_name: &str,
    active: usize,
    revision: u64,
    name: &str,
) -> bool {
    spans_buffer == active && spans_revision == revision && spans_name == name
}

fn bundled_spans_allowed(is_bundled: bool, configured_viewport: bool) -> bool {
    !is_bundled || !configured_viewport
}

fn invalidate_plugin(
    slot: usize,
    generation: u64,
    plugins: &mut [plugin::Plugin],
    mode: &mut Mode,
    viewport_spans: &mut Vec<Vec<(u16, u16, u8)>>,
    spans_owner: &mut Option<(usize, u64)>,
    last_viewport_sent: &mut Option<(usize, u64, u64, u64, u16, u16, String)>,
    dirty: &mut bool,
) {
    if matches!(mode, Mode::PluginWidget { plugin, .. } if *plugin == slot) {
        *mode = Mode::Edit;
    }
    if *spans_owner == Some((slot, generation)) {
        viewport_spans.clear();
        *spans_owner = None;
    }
    plugins[slot].clear_contributions();
    *last_viewport_sent = None;
    *dirty = true;
}

fn lifecycle_code(state: plugin::PluginState) -> u8 {
    match state {
        plugin::PluginState::Starting => 0,
        plugin::PluginState::Running => 1,
        plugin::PluginState::Stopping => 2,
        plugin::PluginState::Backoff => 3,
        plugin::PluginState::Failed => 4,
    }
}

fn sweep_plugin_edges(
    plugins: &mut [plugin::Plugin],
    seen: &mut Vec<(plugin::PluginState, u64)>,
    mode: &mut Mode,
    viewport_spans: &mut Vec<Vec<(u16, u16, u8)>>,
    spans_owner: &mut Option<(usize, u64)>,
    last_viewport_sent: &mut Option<(usize, u64, u64, u64, u16, u16, String)>,
    dirty: &mut bool,
) {
    if seen.len() < plugins.len() {
        seen.resize(plugins.len(), (plugin::PluginState::Failed, 0));
    }
    for i in 0..plugins.len() {
        let old = seen[i];
        let now = (plugins[i].state, plugins[i].process_generation);
        if old.0 == plugin::PluginState::Running && now.0 != plugin::PluginState::Running {
            invalidate_plugin(
                i,
                old.1,
                plugins,
                mode,
                viewport_spans,
                spans_owner,
                last_viewport_sent,
                dirty,
            );
        } else if old.0 != plugin::PluginState::Running && now.0 == plugin::PluginState::Running {
            *last_viewport_sent = None;
            *dirty = true;
        } else if old != now {
            *dirty = true;
        }
        seen[i] = now;
    }
}

#[cfg(test)]
mod startup_tests {
    use super::{
        bundled_highlighter_path, bundled_spans_allowed, configured_highlighter, crash_log_path,
        dispatch_launcher_request, durability_warning, manager_executable, spans_match,
        write_crash_log,
    };
    use crate::plugin;
    use crate::plugin_registry::{
        PluginId, PluginRecord, Registry, RegistryError, RestartPolicy,
        SaveOutcome as RegistrySaveOutcome,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn crash_log_prefers_xdg_state_then_home() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            crash_log_path(p("/s"), p("/h"), 7),
            p("/s/teddy/crash-7.log")
        );
        assert_eq!(
            crash_log_path(p(""), p("/h"), 7),
            p("/h/.local/state/teddy/crash-7.log")
        );
        assert_eq!(
            crash_log_path(p("rel"), p("/h"), 7),
            p("/h/.local/state/teddy/crash-7.log")
        );
        assert_eq!(crash_log_path(None, p("rel"), 7), None);
        assert_eq!(crash_log_path(None, None, 7), None);
    }

    #[test]
    fn crash_log_is_owner_only_and_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("teddy-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = crash_log_path(Some(root.clone()), None, 1).unwrap();
        write_crash_log(&path, "first, longer body").unwrap();
        write_crash_log(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        // umask may only narrow the modes; group/other must never get access
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o077;
        assert_eq!(mode(&path), 0);
        assert_eq!(mode(path.parent().unwrap()), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bundled_highlighter_is_a_sibling_executable() {
        let executable = Path::new("/opt/teddy/bin/teddy");
        assert_eq!(
            bundled_highlighter_path(executable),
            PathBuf::from(format!(
                "/opt/teddy/bin/teddy-highlight{}",
                std::env::consts::EXE_SUFFIX
            ))
        );
    }

    #[test]
    fn explicitly_named_highlighter_is_not_added_again() {
        let bundled = Path::new("/tmp/teddy-highlight");
        assert!(configured_highlighter(
            &[PathBuf::from("teddy-highlight")],
            bundled
        ));
        assert!(!configured_highlighter(
            &[PathBuf::from("other-plugin")],
            bundled
        ));
    }

    #[test]
    fn renamed_buffer_invalidates_spans_even_at_same_revision() {
        assert!(spans_match(2, 7, "old.rs", 2, 7, "old.rs"));
        assert!(!spans_match(2, 7, "old.rs", 2, 7, "notes.md"));
    }

    #[test]
    fn configured_viewport_plugin_wins_over_bundled_spans() {
        assert!(!bundled_spans_allowed(true, true));
        assert!(bundled_spans_allowed(true, false));
        assert!(bundled_spans_allowed(false, true));
    }

    #[test]
    fn manager_path_requires_absolute_regular_executable() {
        assert!(manager_executable(std::ffi::OsStr::new("relative-manager")).is_err());
        assert!(manager_executable(std::ffi::OsStr::new("/bin/sh")).is_ok());
    }

    #[test]
    fn launcher_dispatch_persists_desired_state_before_reporting_success() {
        let path = std::env::temp_dir().join(format!("teddy-dispatch-{}", std::process::id()));
        let mut registry = Registry::load_at(&path).unwrap();
        registry
            .upsert(PluginRecord {
                id: PluginId::new("alpha").unwrap(),
                path: PathBuf::from("/bin/true"),
                enabled: false,
                restart: RestartPolicy::default(),
                generation: 0,
            })
            .unwrap();
        registry.save_at(&path).unwrap();
        let (response, action, warning) = dispatch_launcher_request(
            &mut registry,
            &plugin::LauncherRequest::Enable {
                id: "alpha".into(),
                descriptor: None,
            },
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(response, plugin::LauncherResponse::Enabled("alpha".into()));
        assert_eq!(action, Some(super::LauncherAction::Enable));
        assert!(warning.is_none());
        assert!(registry.list()[0].enabled);
        assert!(Registry::load_at(&path).unwrap().list()[0].enabled);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("teddy-dispatch-{}.lock", std::process::id())),
        );
    }

    #[test]
    fn recovery_is_one_shot_then_stale_saves_conflict() {
        let path = std::env::temp_dir().join(format!("teddy-recovery-{}", std::process::id()));
        std::fs::write(&path, b"corrupt").unwrap();
        let (mut recovered, error) = Registry::load_recoverable_at(&path);
        assert!(error.is_some());
        recovered
            .upsert(PluginRecord {
                id: PluginId::new("alpha").unwrap(),
                path: PathBuf::from("/bin/true"),
                enabled: true,
                restart: RestartPolicy::default(),
                generation: 0,
            })
            .unwrap();
        recovered.save_recovered_at(&path).unwrap();
        let mut stale = Registry::load_at(&path).unwrap();
        recovered.toggle(&PluginId::new("alpha").unwrap()).unwrap();
        recovered.save().unwrap();
        stale.toggle(&PluginId::new("alpha").unwrap()).unwrap();
        assert!(matches!(stale.save(), Err(RegistryError::Conflict)));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("lock"));
    }

    #[test]
    fn committed_registry_warning_keeps_success_path() {
        let warning = durability_warning(RegistrySaveOutcome::CommittedWithWarning {
            error: std::io::Error::new(std::io::ErrorKind::Other, "directory sync warning"),
        });
        assert_eq!(warning.as_deref(), Some("directory sync warning"));
        assert!(durability_warning(RegistrySaveOutcome::Durable).is_none());
    }

    #[test]
    fn tab_lifecycle_labels_new_and_close() {
        use super::{close_tab, new_tab, tab_label};
        use crate::buffer::Buffer;
        let open = |p: &str| Buffer::open(Path::new(p)).unwrap(); // absent: new-file buffers
        let mut bufs = vec![
            open("/nx-teddy/a/mod.rs"),
            open("/nx-teddy/b/mod.rs"),
            open("/nx-teddy/c/x.rs"),
        ];
        assert_eq!(tab_label(&bufs, 0), "a/mod.rs");
        assert_eq!(tab_label(&bufs, 1), "b/mod.rs");
        assert_eq!(tab_label(&bufs, 2), "x.rs");
        // every buffer gets a session-unique revision: a stale plugin frame
        // keyed by a shifted index can never match its new occupant
        assert_ne!(bufs[0].revision, bufs[1].revision);

        assert_eq!(new_tab(&mut bufs), 3);
        assert_eq!(new_tab(&mut bufs), 4);
        assert_eq!(bufs[3].name, "untitled");
        assert_eq!(bufs[4].name, "untitled-2");

        assert_eq!(close_tab(&mut bufs, 0, None), 0);
        assert_eq!(tab_label(&bufs, 0), "mod.rs");
        assert_eq!(close_tab(&mut bufs, 3, None), 2); // last tab: focus moves left
        while bufs.len() > 1 {
            close_tab(&mut bufs, 0, None);
        }
        assert_eq!(close_tab(&mut bufs, 0, None), 0); // never empty
        assert_eq!(bufs.len(), 1);
        assert!(bufs[0].path.is_none());
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
    /// Ctrl+W on a modified tab.
    ConfirmClose,
    /// Statusline text prompt (find / replace flows).
    Prompt(PromptKind),
    /// Interactive replace: a match is highlighted, or the search job is
    /// still hunting for the next one.
    ReplaceConfirm,
    /// Ctrl+P command palette (typed args, suggestion list overlay).
    Palette,
    /// Ctrl+O lazy tree file picker.
    Picker,
    /// Structured list widget provided by an out-of-process plugin.
    PluginWidget {
        plugin: usize,
        widget: u64,
    },
}

/// Palette commands: (name, usage shown in the suggestion list).
const COMMANDS: &[(&str, &str)] = &[
    ("goto", "goto <line>"),
    ("open", "open <path>"),
    ("tab", "tab <n|next|prev>"),
    ("new", "new empty tab (Ctrl+N)"),
    ("close", "close tab (Ctrl+W)"),
    ("save", "save"),
    ("save-as", "save-as <path>"),
    ("reload", "reload from disk"),
    ("follow", "toggle follow mode"),
    ("quit", "quit"),
];

struct PaletteMatch<'a> {
    plugin: Option<usize>,
    name: &'a str,
    usage: &'a str,
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

fn run(buffers: &mut Vec<Buffer>, root: &Path) -> std::io::Result<()> {
    let mut active = 0usize;
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
    let mut picker_state: Option<picker::Picker> = None;
    let mut palette_sel = 0usize;
    let mut pick_top = 0usize;
    let mut widget_sel = 0usize;
    let mut plugins: Vec<plugin::Plugin> = Vec::new();
    let mut next_plugin_request = 1u32;
    let mut viewport_spans: Vec<Vec<(u16, u16, u8)>> = Vec::new();
    let mut spans_revision: u64 = 0;
    let mut spans_buffer: usize = usize::MAX;
    let mut spans_name = String::new();
    let mut spans_owner: Option<(usize, u64)> = None;
    let mut bundled_plugins: Vec<bool> = Vec::new();
    let mut last_viewport_sent: Option<(usize, u64, u64, u64, u16, u16, String)> = None;
    let mut viewport_seq: u32 = 0;
    let registry_path = registry_config_path();
    let (mut registry, registry_error) = registry_path
        .as_deref()
        .map(plugin_registry::Registry::load_recoverable_at)
        .unwrap_or_else(|| (plugin_registry::Registry::default(), None));
    if let Some(error) = registry_error.as_ref() {
        status_msg = format!("plugin registry unavailable: {error}");
        dirty = true;
    }
    let mut registry_recovery_required = registry_error.is_some();
    let mut manager_index: Option<usize> = None;

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
    let mut poll_ready = Vec::new();
    let mut plugin_ready = Vec::new();
    let mut stderr_ready = Vec::new();
    let mut idle_ready = Vec::new();
    // owned tab snapshot, rebuilt only when a name/modified flag changes
    let mut tabs_buf: Vec<(String, bool)> = Vec::new();
    // staleness key for tabs_buf: labels derive from every buffer's name+path
    let mut tabs_key: Vec<(String, Option<PathBuf>)> = Vec::new();
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

    // Durable registry sources precede nonpersistent legacy sources.  The
    // latter retain their historical order, including bundled precedence.
    let mut startup_paths: Vec<(PathBuf, plugin::PluginSource, bool, String)> = registry
        .list()
        .iter()
        .filter(|record| record.enabled)
        .map(|record| {
            (
                record.path.clone(),
                plugin::PluginSource::Registry,
                false,
                record.id.to_string(),
            )
        })
        .collect();
    startup_paths.extend(plugin_paths().into_iter().enumerate().map(
        |(legacy_index, (path, bundled))| {
            (
                path,
                if bundled {
                    plugin::PluginSource::Bundled
                } else {
                    plugin::PluginSource::Legacy
                },
                bundled,
                if bundled {
                    "teddy.bundled.highlight".to_owned()
                } else {
                    format!("teddy.legacy.{legacy_index}")
                },
            )
        },
    ));
    if let Some(value) = std::env::var_os("TEDDY_PLUGIN_MANAGER") {
        match manager_executable(&value) {
            Ok(path) => startup_paths.push((
                path,
                plugin::PluginSource::Manager,
                false,
                "teddy.manager".to_owned(),
            )),
            Err(error) => {
                status_msg = format!("plugin manager unavailable: {error}");
                dirty = true;
            }
        }
    }
    for (path, source, bundled, id) in startup_paths {
        match plugin::Plugin::spawn_with_source_id(&path, source, &id) {
            Ok(mut p) => {
                if source == plugin::PluginSource::Registry {
                    if let Some(record) = registry.list().iter().find(|r| r.id.as_str() == id) {
                        p.max_restarts = record.restart.max_restarts;
                        p.backoff_ms = record.restart.backoff_ms;
                    }
                }
                if source == plugin::PluginSource::Manager {
                    manager_index = Some(plugins.len());
                }
                plugins.push(p);
                bundled_plugins.push(bundled);
            }
            Err(e) => {
                status_msg.clear();
                let _ = write!(status_msg, "{}: plugin failed: {e}", chrome_path(&path));
                dirty = true;
            }
        }
    }
    let mut launcher_frames: Vec<(usize, plugin::Frame)> = Vec::new();
    let mut lifecycle_seen: Vec<(plugin::PluginState, u64)> = plugins
        .iter()
        .map(|p| (p.state, p.process_generation))
        .collect();

    loop {
        for p in &mut plugins {
            p.service();
        }
        sweep_plugin_edges(
            &mut plugins,
            &mut lifecycle_seen,
            &mut mode,
            &mut viewport_spans,
            &mut spans_owner,
            &mut last_viewport_sent,
            &mut dirty,
        );
        // spec §18: input drains before paint (zero timeout while dirty
        // or while cooperative jobs want their next slice)
        let jobs_active = search_job.is_some()
            || replace_job.is_some()
            || picker_state.as_ref().is_some_and(|p| p.wants_step())
            || buffers[active].index_build.is_some();
        let timeout = if dirty || jobs_active {
            0
        } else if parser.has_pending() {
            10
        } else {
            250
        };
        let has_watcher = watcher.is_some();
        let stdin_ready = {
            let mut poll_fds = Vec::new();
            if let Some(w) = watcher.as_ref() {
                poll_fds.push(w.fd());
            }
            for p in &plugins {
                if p.alive {
                    poll_fds.push(p.fd());
                    poll_fds.push(p.stderr_fd());
                }
            }
            term::poll_stdin(&poll_fds, &mut poll_ready, timeout)?
        };
        let watch_ready = has_watcher && poll_ready.first().copied().unwrap_or(false);
        plugin_ready.clear();
        plugin_ready.resize(plugins.len(), false);
        stderr_ready.clear();
        stderr_ready.resize(plugins.len(), false);
        let mut ready_idx = usize::from(has_watcher);
        for (i, p) in plugins.iter().enumerate() {
            if p.alive {
                plugin_ready[i] = poll_ready.get(ready_idx).copied().unwrap_or(false);
                stderr_ready[i] = poll_ready.get(ready_idx + 1).copied().unwrap_or(false);
                ready_idx += 2;
            }
        }
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
                let Some(buf) = buffers.get_mut(t as usize) else {
                    continue;
                };
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

        // Input and watcher work precede all diagnostic output.  The budget
        // is aggregate, so one noisy child cannot monopolize a tick.
        let mut stderr_budget = 16 * 1024usize;
        for i in 0..plugins.len() {
            if stderr_budget == 0 || !stderr_ready.get(i).copied().unwrap_or(false) {
                continue;
            }
            let used = plugins[i].drain_stderr(stderr_budget);
            stderr_budget = stderr_budget.saturating_sub(used);
        }

        for i in 0..plugins.len() {
            if !plugin_ready.get(i).copied().unwrap_or(false) {
                continue;
            }
            let before_count = plugins[i].widgets.len();
            let before_max_revision = plugins[i]
                .widgets
                .values()
                .map(|w| w.revision)
                .max()
                .unwrap_or(0);
            let before_wants_viewport = plugins[i].wants_viewport;
            let frames = plugins[i].pump();
            let mut plugin_dirty = !frames.is_empty();
            if !before_wants_viewport && plugins[i].wants_viewport {
                plugin_dirty = true;
            }
            for frame in frames {
                match frame.msg_type {
                    plugin::LAUNCHER_REQUEST => launcher_frames.push((i, frame)),
                    plugin::EDIT_TX => {
                        let ok = handle_plugin_edit(&frame, buffers);
                        if ok {
                            dirty = true;
                            plugin_dirty = true;
                        }
                        plugins[i].send(&plugin::Frame {
                            msg_type: plugin::EDIT_RESULT,
                            flags: 0,
                            request_id: frame.request_id,
                            resource_id: frame.resource_id,
                            resource_revision: buffers
                                .get(frame.resource_id as usize)
                                .map(|b| b.revision)
                                .unwrap_or(0),
                            payload: vec![u8::from(ok)],
                        });
                    }
                    plugin::SPANS => {
                        let configured_viewport = plugins.iter().enumerate().any(|(index, p)| {
                            !bundled_plugins[index] && p.alive && p.wants_viewport
                        });
                        if bundled_spans_allowed(bundled_plugins[i], configured_viewport)
                            && frame.resource_id == active as u64
                            && frame.resource_revision == buffers[active].revision
                            && frame.request_id == viewport_seq
                        {
                            let editor_rows_now = rows.saturating_sub(2).max(1) as usize;
                            if let Some(rows) = plugin::parse_spans(&frame.payload) {
                                viewport_spans.clear();
                                viewport_spans.resize(editor_rows_now, Vec::new());
                                for (row, spans) in rows {
                                    if let Some(slot) = viewport_spans.get_mut(row as usize) {
                                        *slot = spans;
                                    }
                                }
                                spans_buffer = active;
                                spans_owner = Some((i, plugins[i].process_generation));
                                spans_revision = frame.resource_revision;
                                dirty = true;
                                plugin_dirty = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if plugins[i].widgets.len() != before_count
                || plugins[i]
                    .widgets
                    .values()
                    .any(|w| w.revision > before_max_revision)
            {
                plugin_dirty = true;
                if mode == Mode::Edit {
                    if let Some((&widget, _)) =
                        plugins[i].widgets.iter().max_by_key(|(_, w)| w.revision)
                    {
                        widget_sel = 0;
                        mode = Mode::PluginWidget { plugin: i, widget };
                    }
                }
            }
            if !plugins[i].alive {
                status_msg.clear();
                let _ = write!(status_msg, "{}: plugin stopped", plugins[i].name);
                plugin_dirty = true;
            }
            for notice in plugins[i].notices.drain(..) {
                status_msg = notice;
                plugin_dirty = true;
            }
            if plugin_dirty {
                dirty = true;
            }
        }

        sweep_plugin_edges(
            &mut plugins,
            &mut lifecycle_seen,
            &mut mode,
            &mut viewport_spans,
            &mut spans_owner,
            &mut last_viewport_sent,
            &mut dirty,
        );

        // Launcher control is intentionally handled only after every ready
        // plugin has drained its normal v1 frames for this tick.
        for (source_index, frame) in launcher_frames.drain(..) {
            let authorized =
                manager_index == Some(source_index) && plugins[source_index].manager_capable();
            if !authorized {
                plugins[source_index].stop_for_protocol();
                continue;
            }
            let request = match plugin::decode_launcher_request(&frame) {
                Ok(request) => request,
                Err(_) => {
                    plugins[source_index]
                        .send_launcher_error(frame.request_id, "malformed launcher request");
                    continue;
                }
            };
            let manager_path = plugins[source_index].path.clone();
            if let plugin::LauncherRequest::Forget(id) = &request {
                if let Some(slot) = plugins.iter().find(|p| p.runtime_id == *id) {
                    if slot.state != plugin::PluginState::Failed {
                        plugins[source_index]
                            .send_launcher_error(frame.request_id, "plugin is not fully reaped");
                        continue;
                    }
                }
            }
            match dispatch_launcher_request(
                &mut registry,
                &request,
                Some(&manager_path),
                registry_recovery_required,
                Some(
                    &plugins
                        .iter()
                        .map(|p| (p.runtime_id.clone(), lifecycle_code(p.state)))
                        .collect(),
                ),
            ) {
                Ok((response, action, warning)) => {
                    if let Some(warning) = warning {
                        status_msg = format!(
                            "plugin registry durability warning: {}",
                            plugin::sanitize_stderr(warning.as_bytes())
                        );
                        dirty = true;
                    }
                    if registry_recovery_required
                        && !matches!(request, plugin::LauncherRequest::List { .. })
                    {
                        registry_recovery_required = false;
                    }
                    if let Some(action) = action {
                        let id = match &request {
                            plugin::LauncherRequest::Enable { id, .. }
                            | plugin::LauncherRequest::Disable(id)
                            | plugin::LauncherRequest::Reload(id)
                            | plugin::LauncherRequest::Forget(id) => id,
                            plugin::LauncherRequest::List { .. } => unreachable!(),
                        };
                        let mut runtime_error = None;
                        if let Some(slot) =
                            plugins.iter().position(|plugin| plugin.runtime_id == *id)
                        {
                            if let plugin::LauncherRequest::Enable {
                                descriptor: Some(_),
                                ..
                            } = &request
                            {
                                if let Some(record) =
                                    registry.list().iter().find(|r| r.id.as_str() == id)
                                {
                                    plugins[slot].path = record.path.clone();
                                    plugins[slot].max_restarts = record.restart.max_restarts;
                                    plugins[slot].backoff_ms = record.restart.backoff_ms;
                                    plugins[slot].reset_retry_accounting();
                                }
                            }
                            match action {
                                LauncherAction::Disable | LauncherAction::Forget => {
                                    plugins[slot].stop_for_protocol();
                                    if action == LauncherAction::Forget {
                                        plugins[slot].path = PathBuf::new();
                                    }
                                }
                                LauncherAction::Reload => {
                                    if let Err(error) = plugins[slot].restart() {
                                        runtime_error = Some(error.to_string());
                                    } else {
                                        plugins[slot].reset_retry_accounting();
                                    }
                                }
                                LauncherAction::Enable => {
                                    if !plugins[slot].alive {
                                        if let Err(error) = plugins[slot].restart() {
                                            runtime_error = Some(error.to_string());
                                        }
                                    }
                                }
                            }
                        } else if matches!(action, LauncherAction::Enable) {
                            if let Some(record) =
                                registry.list().iter().find(|r| r.id.as_str() == id)
                            {
                                match plugin::Plugin::spawn_with_source_id(
                                    &record.path,
                                    plugin::PluginSource::Registry,
                                    id,
                                ) {
                                    Ok(plugin) => {
                                        let mut plugin = plugin;
                                        plugin.max_restarts = record.restart.max_restarts;
                                        plugin.backoff_ms = record.restart.backoff_ms;
                                        plugins.push(plugin);
                                        bundled_plugins.push(false);
                                    }
                                    Err(error) => runtime_error = Some(error.to_string()),
                                }
                            }
                        } else if matches!(action, LauncherAction::Reload) {
                            runtime_error = Some("plugin is not running".into());
                        }
                        if let Some(error) = runtime_error {
                            plugins[source_index].send_launcher_error(frame.request_id, &error);
                            continue;
                        }
                        let event = match action {
                            LauncherAction::Enable => plugin::LauncherEvent::Enabled(id.clone()),
                            LauncherAction::Disable => plugin::LauncherEvent::Disabled(id.clone()),
                            LauncherAction::Reload => plugin::LauncherEvent::Reloaded(id.clone()),
                            LauncherAction::Forget => plugin::LauncherEvent::Forgotten(id.clone()),
                        };
                        if let Ok(event_frame) = plugin::encode_launcher_event(&event) {
                            plugins[source_index].send(&event_frame);
                        }
                    }
                    match plugin::encode_launcher_response(frame.request_id, &response) {
                        Ok(response_frame) => plugins[source_index].send(&response_frame),
                        Err(_) => plugins[source_index]
                            .send_launcher_error(frame.request_id, "launcher response too large"),
                    }
                }
                Err(error) => plugins[source_index].send_launcher_error(frame.request_id, &error),
            }
        }

        sweep_plugin_edges(
            &mut plugins,
            &mut lifecycle_seen,
            &mut mode,
            &mut viewport_spans,
            &mut spans_owner,
            &mut last_viewport_sent,
            &mut dirty,
        );

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
                Mode::ConfirmClose => {
                    mode = Mode::Edit;
                    let close = match k {
                        Key::Char('y') | Key::Char('Y') => true,
                        Key::Char('s') | Key::Char('S') => match buffers[active].save(false) {
                            Ok(()) => true,
                            Err(e) => {
                                let _ = write!(status_msg, "save failed: {e} — use save-as");
                                false
                            }
                        },
                        _ => false,
                    };
                    if close {
                        search_job = None;
                        replace_job = None;
                        confirm_match = None;
                        pending_force_save = false;
                        active = close_tab(buffers, active, watcher.as_mut());
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
                                    replace_scope_end = (replace_scope_end as i64 + delta) as u64;
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
                            search_job =
                                Some(search::Search::new_no_wrap(find_needle.clone(), at + 1));
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
                Mode::Palette => {
                    match k {
                        Key::Esc => {
                            prompt.clear();
                            mode = Mode::Edit;
                        }
                        Key::Char(c) if prompt.len() < 4096 => prompt.push(c),
                        Key::Char(_) => {}
                        Key::Backspace => {
                            prompt.pop();
                        }
                        Key::Up => palette_sel = palette_sel.saturating_sub(1),
                        Key::Down => palette_sel += 1,
                        Key::Tab => {
                            let tok = prompt.split_whitespace().next().unwrap_or("");
                            let mut matches = Vec::new();
                            collect_palette_matches(tok, prompt.is_empty(), &plugins, &mut matches);
                            if !matches.is_empty() {
                                let i = palette_sel.min(matches.len() - 1);
                                prompt.clear();
                                prompt.push_str(matches[i].name);
                                prompt.push(' ');
                            }
                        }
                        Key::Enter => {
                            let line = std::mem::take(&mut prompt);
                            mode = Mode::Edit;
                            let mut parts = line.splitn(2, char::is_whitespace);
                            let tok = parts.next().unwrap_or("");
                            let arg = parts.next().unwrap_or("").trim();
                            let cmd = resolve_builtin_command(tok);
                            match cmd {
                                Some("goto") => {
                                    if buffers[active].line_index.is_none() {
                                        status_msg.push_str("no line index yet");
                                    } else if let Ok(n) = arg.parse::<u64>() {
                                        let buf = &mut buffers[active];
                                        let n = n.clamp(1, buf.line_count());
                                        buf.cursor = buf.line_start(n - 1);
                                        buf.goal_col = 0;
                                        buf.sel_anchor = None;
                                    } else {
                                        status_msg.push_str("goto <line>");
                                    }
                                }
                                Some("open") => {
                                    if arg.is_empty() {
                                        status_msg.push_str("open <path>");
                                    } else {
                                        let path = PathBuf::from(arg);
                                        let path = if path.is_absolute() {
                                            path
                                        } else {
                                            root.join(path)
                                        };
                                        match Buffer::open(&path) {
                                            Ok(b) => {
                                                search_job = None;
                                                replace_job = None;
                                                confirm_match = None;
                                                buffers.push(b);
                                                active = buffers.len() - 1;
                                                if let (Some(w), Some(p)) =
                                                    (watcher.as_mut(), buffers[active].path.clone())
                                                {
                                                    let _ = w.watch(active as u64, &p);
                                                }
                                            }
                                            Err(e) => {
                                                let _ = write!(status_msg, "{arg}: {e}");
                                            }
                                        }
                                    }
                                }
                                Some("tab") => match arg {
                                    "next" => {
                                        search_job = None;
                                        replace_job = None;
                                        confirm_match = None;
                                        active = (active + 1) % buffers.len();
                                    }
                                    "prev" => {
                                        search_job = None;
                                        replace_job = None;
                                        confirm_match = None;
                                        active = (active + buffers.len() - 1) % buffers.len();
                                    }
                                    _ => {
                                        if let Ok(n) = arg.parse::<usize>() {
                                            if (1..=buffers.len()).contains(&n) {
                                                search_job = None;
                                                replace_job = None;
                                                confirm_match = None;
                                                active = n - 1;
                                            } else {
                                                status_msg.push_str("tab <n|next|prev>");
                                            }
                                        } else {
                                            status_msg.push_str("tab <n|next|prev>");
                                        }
                                    }
                                },
                                Some("new") => {
                                    search_job = None;
                                    replace_job = None;
                                    confirm_match = None;
                                    active = new_tab(buffers);
                                }
                                Some("close") => {
                                    if buffers[active].modified() {
                                        mode = Mode::ConfirmClose;
                                    } else {
                                        search_job = None;
                                        replace_job = None;
                                        confirm_match = None;
                                        active = close_tab(buffers, active, watcher.as_mut());
                                    }
                                }
                                Some("save") => {
                                    let buf = &mut buffers[active];
                                    match buf.save(false) {
                                        Ok(()) => {
                                            let _ = write!(status_msg, "saved {}", buf.name);
                                            if let (Some(w), Some(p)) =
                                                (watcher.as_mut(), buf.path.clone())
                                            {
                                                let _ = w.watch(active as u64, &p);
                                            }
                                        }
                                        Err(e) => {
                                            let _ = write!(status_msg, "save failed: {e}");
                                        }
                                    }
                                }
                                Some("save-as") => {
                                    if arg.is_empty() {
                                        status_msg.push_str("save-as <path>");
                                    } else {
                                        let path = PathBuf::from(arg);
                                        let path = if path.is_absolute() {
                                            path
                                        } else {
                                            root.join(path)
                                        };
                                        let buf = &mut buffers[active];
                                        match buf.save_as(path) {
                                            Ok(()) => {
                                                let _ = write!(status_msg, "saved {}", buf.name);
                                                if let (Some(w), Some(p)) =
                                                    (watcher.as_mut(), buf.path.clone())
                                                {
                                                    let _ = w.watch(active as u64, &p);
                                                }
                                            }
                                            Err(e) => {
                                                let _ = write!(status_msg, "save failed: {e}");
                                            }
                                        }
                                    }
                                }
                                Some("reload") => {
                                    if buffers[active].modified() {
                                        status_msg.push_str("unsaved changes — save or undo first");
                                    } else {
                                        match buffers[active].reload() {
                                            Ok(()) => {
                                                status_msg.push_str("reloaded");
                                                search_job = None;
                                                replace_job = None;
                                                confirm_match = None;
                                            }
                                            Err(e) => {
                                                let _ = write!(status_msg, "{e}");
                                            }
                                        }
                                    }
                                }
                                Some("follow") => {
                                    let buf = &mut buffers[active];
                                    if !buf.follow && buf.modified() {
                                        status_msg.push_str("unsaved changes — save before follow");
                                    } else if !buf.follow {
                                        buf.follow = true;
                                        buf.readonly = true;
                                        buf.cursor = buf.len();
                                        status_msg.push_str("follow on");
                                    } else {
                                        buf.follow = false;
                                        buf.readonly = buf.binary;
                                        status_msg.push_str("follow off");
                                    }
                                }
                                Some("quit") => {
                                    if buffers.iter().any(|b| b.modified()) {
                                        mode = Mode::ConfirmQuit;
                                    } else {
                                        return Ok(());
                                    }
                                }
                                Some(_) => {
                                    let _ = write!(status_msg, "unknown command: {tok}");
                                }
                                None => {
                                    if let Some(pi) = resolve_plugin_command(tok, &plugins) {
                                        let request_id = next_plugin_request;
                                        next_plugin_request =
                                            next_plugin_request.wrapping_add(1).max(1);
                                        plugins[pi].send(&plugin::Frame {
                                            msg_type: plugin::COMMAND_INVOKE,
                                            flags: 0,
                                            request_id,
                                            resource_id: active as u64,
                                            resource_revision: buffers[active].revision,
                                            payload: arg.as_bytes().to_vec(),
                                        });
                                    } else {
                                        let _ = write!(status_msg, "unknown command: {tok}");
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                Mode::Picker => {
                    let p = picker_state.as_mut().expect("picker mode without picker");
                    match k {
                        Key::Esc => {
                            picker_state = None;
                            mode = Mode::Edit;
                        }
                        Key::Char(c) => {
                            p.filter.push(c);
                            p.filter_changed();
                        }
                        Key::Backspace => {
                            p.filter.pop();
                            p.filter_changed();
                        }
                        Key::Up => p.sel = p.sel.saturating_sub(1),
                        Key::Down => p.sel = (p.sel + 1).min(p.entries.len().saturating_sub(1)),
                        Key::Enter => {
                            if let Some(path) = p.activate() {
                                match Buffer::open(&path) {
                                    Ok(b) => {
                                        search_job = None;
                                        replace_job = None;
                                        confirm_match = None;
                                        buffers.push(b);
                                        active = buffers.len() - 1;
                                        if let (Some(w), Some(p)) =
                                            (watcher.as_mut(), buffers[active].path.clone())
                                        {
                                            let _ = w.watch(active as u64, &p);
                                        }
                                        picker_state = None;
                                        mode = Mode::Edit;
                                    }
                                    Err(e) => {
                                        let _ = write!(status_msg, "{}: {e}", chrome_path(&path));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                Mode::PluginWidget { plugin: pi, widget } => {
                    let item_count = plugins
                        .get(pi)
                        .and_then(|p| p.widgets.get(&widget))
                        .map(|w| w.items.len())
                        .unwrap_or(0);
                    match k {
                        Key::Esc => mode = Mode::Edit,
                        Key::Up => widget_sel = widget_sel.saturating_sub(1),
                        Key::Down => {
                            widget_sel = (widget_sel + 1).min(item_count.saturating_sub(1));
                        }
                        Key::Enter => {
                            if item_count > 0 {
                                widget_sel = widget_sel.min(item_count - 1);
                                if let Some(p) = plugins.get_mut(pi) {
                                    p.send(&plugin::Frame {
                                        msg_type: plugin::WIDGET_EVENT,
                                        flags: 0,
                                        request_id: 0,
                                        resource_id: widget,
                                        resource_revision: p
                                            .widgets
                                            .get(&widget)
                                            .map(|w| w.revision)
                                            .unwrap_or(0),
                                        payload: (widget_sel as u32).to_le_bytes().to_vec(),
                                    });
                                }
                            }
                        }
                        _ => {}
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
                Key::Ctrl(b'P') => {
                    last_edit_kind = KIND_NONE;
                    prompt.clear();
                    palette_sel = 0;
                    mode = Mode::Palette;
                }
                Key::Ctrl(b'O') => {
                    last_edit_kind = KIND_NONE;
                    picker_state = Some(picker::Picker::new(root.to_path_buf()));
                    pick_top = 0;
                    mode = Mode::Picker;
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
                Key::Ctrl(b'N') => {
                    last_edit_kind = KIND_NONE;
                    replace_job = None;
                    confirm_match = None;
                    active = new_tab(buffers);
                }
                Key::Ctrl(b'W') => {
                    last_edit_kind = KIND_NONE;
                    if buffers[active].modified() {
                        mode = Mode::ConfirmClose;
                    } else {
                        replace_job = None;
                        confirm_match = None;
                        active = close_tab(buffers, active, watcher.as_mut());
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
                    do_edit(
                        &mut buffers[active],
                        s.as_bytes(),
                        KIND_INSERT,
                        &mut last_edit_kind,
                        &mut status_msg,
                        &mut scratch,
                    );
                }
                Key::Enter => do_edit(
                    &mut buffers[active],
                    b"\n",
                    KIND_INSERT,
                    &mut last_edit_kind,
                    &mut status_msg,
                    &mut scratch,
                ),
                Key::Tab => do_edit(
                    &mut buffers[active],
                    b"\t",
                    KIND_INSERT,
                    &mut last_edit_kind,
                    &mut status_msg,
                    &mut scratch,
                ),
                Key::Backspace => do_delete(
                    &mut buffers[active],
                    false,
                    &mut last_edit_kind,
                    &mut status_msg,
                    &mut scratch,
                ),
                Key::Delete => do_delete(
                    &mut buffers[active],
                    true,
                    &mut last_edit_kind,
                    &mut status_msg,
                    &mut scratch,
                ),
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
        if !term::poll_stdin(&[], &mut idle_ready, 0)? {
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
                            let _ =
                                write!(status_msg, "replaced {replace_count} — end of selection");
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
                            let _ =
                                write!(status_msg, "replaced {replace_count} — no more matches");
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
            } else if picker_state.as_ref().is_some_and(|p| p.wants_step()) {
                picker_state.as_mut().unwrap().step(64);
                dirty = true;
            } else if buffers[active].index_build.is_some() {
                buffers[active].step_index_build(4 * 1024 * 1024, &mut job_scratch);
                dirty = true;
            }
        }

        if dirty && !term::poll_stdin(&[], &mut idle_ready, 0)? {
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
                Mode::ConfirmClose => {
                    status_left.clear();
                    status_left.push_str(" Unsaved changes — y: close  s: save & close  n: back");
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
                Mode::Palette => {
                    status_left.clear();
                    let _ = write!(status_left, " > {prompt}");
                    let tok = prompt.split_whitespace().next().unwrap_or("");
                    let mut matches = Vec::new();
                    collect_palette_matches(tok, prompt.is_empty(), &plugins, &mut matches);
                    if !matches.is_empty() {
                        palette_sel = palette_sel.min(matches.len() - 1);
                    }
                    let shown = matches.len().min(8).min(editor_rows as usize);
                    let base = editor_rows as usize - shown;
                    for (i, m) in matches.iter().take(shown).enumerate() {
                        let row = &mut row_store[base + i];
                        row.clear();
                        // ponytail: command palette paints are not a hot path.
                        if let Some(pi) = m.plugin {
                            row.extend_from_slice(
                                format!("{}  — plugin: {}", m.name, plugins[pi].name).as_bytes(),
                            );
                        } else {
                            row.extend_from_slice(format!("{}  — {}", m.name, m.usage).as_bytes());
                        }
                        row_sel[base + i] = if i == palette_sel {
                            Some((0, row.len()))
                        } else {
                            None
                        };
                    }
                }
                Mode::Picker => {
                    let p = picker_state.as_ref().unwrap();
                    status_left.clear();
                    let _ = write!(status_left, " pick: {}", p.filter);
                    if p.wants_step() {
                        status_left.push_str("  (searching…)");
                    }
                    status_right.clear();
                    let _ = write!(status_right, "{} entries  Esc cancel ", p.entries.len());
                    if p.sel < pick_top {
                        pick_top = p.sel;
                    }
                    if p.sel >= pick_top + editor_rows as usize {
                        pick_top = p.sel + 1 - editor_rows as usize;
                    }
                    for i in 0..editor_rows as usize {
                        row_store[i].clear();
                        row_sel[i] = None;
                        if let Some(e) = p.entries.get(pick_top + i) {
                            row_store[i].extend(std::iter::repeat(b' ').take(2 * e.depth));
                            row_store[i].extend_from_slice(e.name.as_bytes());
                            if e.is_dir {
                                row_store[i].push(b'/');
                            }
                            if pick_top + i == p.sel {
                                row_sel[i] = Some((0, row_store[i].len()));
                            }
                        }
                    }
                }
                Mode::PluginWidget { plugin: pi, widget } => {
                    status_left.clear();
                    if let Some(p) = plugins.get(pi) {
                        let _ = write!(status_left, " {} widget  Enter select  Esc close", p.name);
                    } else {
                        status_left.push_str(" plugin widget  Enter select  Esc close");
                    }
                    for i in 0..editor_rows as usize {
                        row_store[i].clear();
                        row_sel[i] = None;
                        if let Some(item) = plugins
                            .get(pi)
                            .and_then(|p| p.widgets.get(&widget))
                            .and_then(|w| w.items.get(i))
                        {
                            row_store[i].extend_from_slice(item.as_bytes());
                            if i == widget_sel {
                                row_sel[i] = Some((0, row_store[i].len()));
                            }
                        }
                    }
                }
                _ => {
                    if let Some(s) = &search_job {
                        let _ = write!(status_left, "  searching… {}%", s.progress(buf.len()));
                    } else if let Some(j) = &replace_job {
                        let _ = write!(status_left, "  replacing… {}%", j.progress(buf.len()));
                    } else if let Some(ib) = &buf.index_build {
                        let pct = if buf.len() == 0 {
                            100
                        } else {
                            ib.pos * 100 / buf.len()
                        };
                        let _ = write!(status_left, "  indexing… {pct}%");
                    }
                    if !status_msg.is_empty() {
                        let _ = write!(status_left, "  — {status_msg}");
                    }
                }
            }
            // rebuild the owned tab snapshot only on change (no per-paint alloc)
            let tabs_stale =
                tabs_key.len() != buffers.len()
                    || tabs_buf.iter().zip(&tabs_key).zip(buffers.iter()).any(
                        |((t, (n, p)), b)| t.1 != b.modified() || *n != b.name || *p != b.path,
                    );
            if tabs_stale {
                tabs_buf.clear();
                tabs_buf.extend(
                    (0..buffers.len()).map(|i| (tab_label(buffers, i), buffers[i].modified())),
                );
                tabs_key.clear();
                tabs_key.extend(buffers.iter().map(|b| (b.name.clone(), b.path.clone())));
            }
            sweep_plugin_edges(
                &mut plugins,
                &mut lifecycle_seen,
                &mut mode,
                &mut viewport_spans,
                &mut spans_owner,
                &mut last_viewport_sent,
                &mut dirty,
            );
            if !spans_match(
                spans_buffer,
                spans_revision,
                &spans_name,
                active,
                buffers[active].revision,
                &buffers[active].name,
            ) {
                viewport_spans.clear();
                spans_buffer = usize::MAX;
                spans_revision = 0;
                spans_name = buffers[active].name.clone();
            }
            let valid_spans = spans_match(
                spans_buffer,
                spans_revision,
                &spans_name,
                active,
                buffers[active].revision,
                &buffers[active].name,
            );
            let v = View {
                cols,
                rows,
                tabs: &tabs_buf,
                active_tab: active,
                row_bytes: &row_store,
                row_sel: &row_sel,
                row_spans: if valid_spans { &viewport_spans } else { &[] },
                left_col: buffers[active].left_col,
                cursor_screen,
                status_left: &status_left,
                status_right: &status_right,
            };
            render::paint(&mut frame, &mut render_cache, &v);
            out.write_all(frame.as_bytes())?;
            out.flush()?;
            dirty = false;
            let viewport_key = (
                active,
                buffers[active].revision,
                buffers[active].top_line,
                buffers[active].top_byte,
                cols,
                rows,
                buffers[active].name.clone(),
            );
            if mode == Mode::Edit
                && last_viewport_sent.as_ref() != Some(&viewport_key)
                && plugins.iter().any(|p| p.alive && p.wants_viewport)
            {
                // Overlays own row_store outside Edit mode, so VIEWPORT frames are only
                // sent for real buffer rows. Rows are clipped again in the protocol payload.
                viewport_seq = viewport_seq.wrapping_add(1);
                let payload = viewport_payload(&buffers[active].name, &row_store);
                for p in plugins.iter_mut().filter(|p| p.alive && p.wants_viewport) {
                    p.send(&plugin::Frame {
                        msg_type: plugin::VIEWPORT,
                        flags: 0,
                        request_id: viewport_seq,
                        resource_id: active as u64,
                        resource_revision: buffers[active].revision,
                        payload: payload.clone(),
                    });
                }
                last_viewport_sent = Some(viewport_key);
            }
            sweep_plugin_edges(
                &mut plugins,
                &mut lifecycle_seen,
                &mut mode,
                &mut viewport_spans,
                &mut spans_owner,
                &mut last_viewport_sent,
                &mut dirty,
            );
            #[cfg(feature = "perf")]
            perf.frame_end(frame.as_bytes().len());
        }
    }
}

fn collect_palette_matches<'a>(
    tok: &str,
    include_all: bool,
    plugins: &'a [plugin::Plugin],
    out: &mut Vec<PaletteMatch<'a>>,
) {
    out.clear();
    out.extend(
        COMMANDS
            .iter()
            .filter(|(name, _)| include_all || name.starts_with(tok))
            .map(|(name, usage)| PaletteMatch {
                plugin: None,
                name,
                usage,
            }),
    );
    for (pi, p) in plugins.iter().enumerate() {
        for name in &p.commands {
            if include_all || name.starts_with(tok) {
                out.push(PaletteMatch {
                    plugin: Some(pi),
                    name,
                    usage: "",
                });
            }
        }
    }
}

/// Tab label: the buffer name, or `parent/name` when another tab shares it.
/// ponytail: one parent level; same parent name in two trees still collides.
fn tab_label(buffers: &[Buffer], i: usize) -> String {
    use std::os::unix::ffi::OsStrExt;
    let b = &buffers[i];
    let dup = buffers
        .iter()
        .enumerate()
        .any(|(j, o)| j != i && o.name == b.name);
    let parent = b
        .path
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::file_name);
    match parent {
        Some(parent) if dup => format!("{}/{}", render::escape_name(parent.as_bytes()), b.name),
        _ => b.name.clone(),
    }
}

/// Ctrl+N: append an empty pathless tab; returns its index.
fn new_tab(buffers: &mut Vec<Buffer>) -> usize {
    let mut b = Buffer::untitled();
    let mut n = 1;
    while buffers.iter().any(|o| o.name == b.name) {
        n += 1;
        b.name = format!("untitled-{n}");
    }
    buffers.push(b);
    buffers.len() - 1
}

/// Remove tab `k`, keeping at least one (an untitled) buffer. Watch tokens
/// are buffer indices, so every shifted buffer is re-armed under its new
/// index; plugin frames keyed by index are fenced by session-unique
/// revisions. Returns the new active index.
fn close_tab(buffers: &mut Vec<Buffer>, k: usize, watcher: Option<&mut watch::Watcher>) -> usize {
    let old_len = buffers.len();
    buffers.remove(k);
    if buffers.is_empty() {
        buffers.push(Buffer::untitled());
    }
    if let Some(w) = watcher {
        for t in k..old_len {
            w.unwatch(t as u64);
        }
        for (t, b) in buffers.iter().enumerate().skip(k) {
            if let Some(p) = &b.path {
                let _ = w.watch(t as u64, p); // new-file buffers: not on disk yet
            }
        }
    }
    k.min(buffers.len() - 1)
}

fn resolve_builtin_command(tok: &str) -> Option<&'static str> {
    if tok.is_empty() {
        return None;
    }
    if let Some((name, _)) = COMMANDS.iter().find(|(name, _)| *name == tok) {
        return Some(*name);
    }
    let mut hits = COMMANDS.iter().filter(|(name, _)| name.starts_with(tok));
    let first = hits.next().map(|(name, _)| *name);
    if first.is_some() && hits.next().is_none() {
        first
    } else {
        None
    }
}

fn resolve_plugin_command(tok: &str, plugins: &[plugin::Plugin]) -> Option<usize> {
    if tok.is_empty() {
        return None;
    }
    let mut hit = None;
    for (pi, p) in plugins.iter().enumerate() {
        for name in &p.commands {
            if name == tok || name.starts_with(tok) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(pi);
            }
        }
    }
    hit
}

fn handle_plugin_edit(frame: &plugin::Frame, buffers: &mut [Buffer]) -> bool {
    let Ok(idx) = usize::try_from(frame.resource_id) else {
        return false;
    };
    let Some(buf) = buffers.get_mut(idx) else {
        return false;
    };
    if frame.resource_revision != buf.revision || frame.payload.len() < 16 || buf.readonly {
        return false;
    }

    let start = u64::from_le_bytes(frame.payload[0..8].try_into().unwrap());
    let end = u64::from_le_bytes(frame.payload[8..16].try_into().unwrap());
    if start > end || end > buf.len() {
        return false;
    }

    buf.group_counter += 1;
    let group = buf.group_counter;
    buf.replace(start, end, &frame.payload[16..], group).is_ok()
}

fn viewport_payload(name: &str, rows: &[Vec<u8>]) -> Vec<u8> {
    let name = name.as_bytes();
    let name_len = name.len().min(u16::MAX as usize);
    let row_count = rows.len().min(u16::MAX as usize);
    let mut payload = Vec::with_capacity(2 + name_len + 2 + row_count * 16);
    payload.extend_from_slice(&(name_len as u16).to_le_bytes());
    payload.extend_from_slice(&name[..name_len]);
    payload.extend_from_slice(&(row_count as u16).to_le_bytes());
    for row in rows.iter().take(row_count) {
        let len = row.len().min(4096).min(u16::MAX as usize);
        payload.extend_from_slice(&(len as u16).to_le_bytes());
        payload.extend_from_slice(&row[..len]);
    }
    payload
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
            Perf {
                log,
                t0: None,
                allocs0: 0,
            }
        }

        pub fn frame_start(&mut self) {
            self.t0 = Some(Instant::now());
            self.allocs0 = ALLOCS.load(Ordering::Relaxed);
        }

        pub fn frame_end(&mut self, frame_bytes: usize) {
            let (Some(t0), Some(log)) = (self.t0.take(), self.log.as_mut()) else {
                return;
            };
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

fn do_delete(
    buf: &mut Buffer,
    forward: bool,
    last_kind: &mut u8,
    msg: &mut String,
    scratch: &mut Vec<u8>,
) {
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
        buf.read_range(
            row_start,
            (buf.cursor - row_start).min(LINE_CAP as u64),
            scratch,
        );
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
    buf.read_range(
        line_start,
        (buf.cursor - line_start).min(LINE_CAP as u64),
        scratch,
    );
    let vcol = render::visual_col(scratch, scratch.len());
    clamp_left(buf, vcol, cols);
    (
        (vcol - buf.left_col) as u16,
        (cursor_line - buf.top_line) as u16,
    )
}

fn intersect_sel(
    sel: Option<(u64, u64)>,
    row_start: u64,
    row_len: usize,
) -> Option<(usize, usize)> {
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

/// Path for the statusline: raw bytes, control/invalid ones as `\xNN`.
fn chrome_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    render::escape_name(path.as_os_str().as_bytes())
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
        let pct = if buf.len() == 0 {
            100
        } else {
            buf.cursor * 100 / buf.len()
        };
        let _ = write!(
            right,
            "byte {} / {} ({}%)  Ctrl+Q quit ",
            buf.cursor,
            buf.len(),
            pct
        );
    } else {
        let line = buf.line_of_byte(buf.cursor);
        let _ = write!(
            right,
            "Ln {}, Col {}  Ctrl+Q quit ",
            line + 1,
            buf.goal_col + 1
        );
    }
}
