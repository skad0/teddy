#[path = "../plugin.rs"]
mod plugin;
#[path = "../plugin_manager.rs"]
mod plugin_manager;

use plugin::{
    decode_launcher_event, decode_launcher_response, encode, encode_launcher_request, Frame,
    FrameReader, LauncherDescriptor, LauncherRequest, LauncherResponse, COMMAND_INVOKE, HELLO,
    LAUNCHER_EVENT, LAUNCHER_RESPONSE, PROTO_VERSION, REGISTER_COMMAND, STATUS, WIDGET,
    WIDGET_EVENT,
};
use plugin_manager::{
    advance_update, begin_update, install_git, payload_digest, pending_update, read_receipt,
    recover_manager, CatalogEntry, ManagerLock, ManagerPaths, PluginManifest, Receipt,
    RemovalResult, UpdateIdentity, UpdateStage, UpdateTransaction,
};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};

const CATALOG: u64 = 0x4D01;
const INSTALLED: u64 = 0x4D02;
const CORE: u64 = 0x4D03;
const ACTIONS: u64 = 0x4D10;
const CONFIRM: u64 = 0x4D11;
const NOTICE_LIMIT: usize = 160;
const MAX_CATALOG_BYTES: usize = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 1024;
const MAX_VERSION_ENTRIES: usize = 1024;
const MAX_INSTALLED_SCAN_WORK: usize = 2048;
const INSTALLED_ROWS_PER_RECEIPT: usize = 6;
const MAX_WIDGET_ROWS: usize = 256;
const MAX_ROW_BYTES: usize = 2048;
const MAX_WIDGET_PAYLOAD: usize = plugin::MAX_PAYLOAD as usize;
const MAX_LIST_PAGES: usize = 256;
const MAX_LIST_RECORDS: usize = 1024;
const MAX_LIST_PAGE_RECORDS: usize = 32;
const MAX_STATE_POLLS: usize = 3;
const MANAGER_ID: &str = "teddy.manager";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Catalog,
    Installed,
    Core,
}

#[derive(Clone, Debug)]
struct Resource {
    revision: u64,
    rows: Vec<String>,
    ids: Vec<Option<String>>,
}

impl Resource {
    fn new(_id: u64) -> Self {
        Self {
            revision: 0,
            rows: Vec::new(),
            ids: Vec::new(),
        }
    }
    fn replace(&mut self, rows: Vec<String>, ids: Vec<Option<String>>) {
        self.revision += 1;
        self.rows = rows;
        self.ids = ids;
    }
    fn valid_selection(&self, revision: u64, index: usize) -> Option<&str> {
        (revision == self.revision).then_some(())?;
        self.ids.get(index)?.as_deref()
    }
}

#[derive(Debug)]
struct PendingList {
    expected_page: u16,
    pages: usize,
    records: usize,
    collected: Vec<plugin::LauncherRecord>,
    purpose: ListPurpose,
}

#[derive(Debug, Clone)]
enum ListPurpose {
    Core,
    Lifecycle { target: Target, poll: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Install,
    InstallEnable,
    Enable,
    Disable,
    Remove,
    Update,
}

#[derive(Debug, Clone)]
struct Confirmation {
    source: u64,
    source_revision: u64,
    target: Target,
    update_old: Option<Target>,
}

#[derive(Debug, Clone)]
struct ActiveUpdate {
    transaction: UpdateTransaction,
    old: Target,
    candidate: Target,
}

#[derive(Debug, Clone)]
struct Control {
    id: String,
    action: Action,
    target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetSource {
    Catalog {
        repository: String,
        executable: String,
        platform: String,
    },
    Receipt {
        executable: String,
        digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    action: Action,
    id: String,
    commit: String,
    source: TargetSource,
    receipt: Option<ReceiptIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptIdentity {
    executable: String,
    digest: String,
}

#[derive(Debug)]
struct InstalledBudget {
    used: usize,
    truncated: bool,
}

impl InstalledBudget {
    fn new() -> Self {
        Self {
            used: 0,
            truncated: false,
        }
    }
    fn take(&mut self, units: usize) -> bool {
        if self.used.saturating_add(units) > MAX_INSTALLED_SCAN_WORK {
            self.truncated = true;
            return false;
        }
        self.used += units;
        true
    }
}

fn main() -> io::Result<()> {
    let mut out = io::stdout().lock();
    let mut input = io::stdin().lock();
    let mut reader = FrameReader::new();
    let mut bytes = [0u8; 8192];
    let mut app = App::new();
    loop {
        let n = input.read(&mut bytes)?;
        if n == 0 {
            return Ok(());
        }
        reader.push(&bytes[..n]);
        while let Some(frame) = reader.next().map_err(proto_io)? {
            app.handle(frame, &mut out)?;
        }
    }
}

struct App {
    paths: Option<ManagerPaths>,
    resources: HashMap<u64, Resource>,
    active: Option<View>,
    next_request: u32,
    pending_lists: HashMap<u32, PendingList>,
    core_records: Vec<plugin::LauncherRecord>,
    confirmations: Option<Confirmation>,
    controls: HashMap<u32, Control>,
    active_update: Option<ActiveUpdate>,
}

impl App {
    fn new() -> Self {
        let mut resources = HashMap::new();
        for id in [CATALOG, INSTALLED, CORE, ACTIONS, CONFIRM] {
            resources.insert(id, Resource::new(id));
        }
        Self {
            paths: ManagerPaths::from_env().ok(),
            resources,
            active: None,
            next_request: 1,
            pending_lists: HashMap::new(),
            core_records: Vec::new(),
            confirmations: None,
            controls: HashMap::new(),
            active_update: None,
        }
    }

    fn handle<W: Write>(&mut self, frame: Frame, out: &mut W) -> io::Result<()> {
        match frame.msg_type {
            HELLO => {
                if frame.payload != PROTO_VERSION.to_le_bytes() {
                    return Err(proto_io(plugin::ProtoError::Malformed));
                }
                send(
                    out,
                    Frame {
                        msg_type: HELLO,
                        flags: 0,
                        request_id: 0,
                        resource_id: 0,
                        resource_revision: 0,
                        payload: frame.payload,
                    },
                )?;
                for command in [
                    "plugins-catalog",
                    "plugins-installed",
                    "plugins-core",
                    "plugins-refresh",
                ] {
                    send(
                        out,
                        Frame {
                            msg_type: REGISTER_COMMAND,
                            flags: 0,
                            request_id: 0,
                            resource_id: 0,
                            resource_revision: 0,
                            payload: command.as_bytes().to_vec(),
                        },
                    )?;
                }
                self.refresh(out)?;
            }
            COMMAND_INVOKE => {
                let command = String::from_utf8_lossy(&frame.payload);
                let (name, query) = parse_command(&command);
                match name {
                    "plugins-catalog" => {
                        self.active = Some(View::Catalog);
                        self.show_catalog(query, out)?;
                    }
                    "plugins-installed" => {
                        self.active = Some(View::Installed);
                        self.show_installed(query, out)?;
                    }
                    "plugins-core" => {
                        self.active = Some(View::Core);
                        self.show_core(query, out)?;
                    }
                    "plugins-refresh" => self.refresh(out)?,
                    _ => self.notice(out, "Unknown manager command."),
                }
            }
            WIDGET_EVENT => self.selection(frame, out)?,
            LAUNCHER_EVENT => self.launcher_event(frame, out)?,
            LAUNCHER_RESPONSE => self.launcher_response(frame, out)?,
            _ => {}
        }
        Ok(())
    }

    fn refresh<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.notice(out, "Refreshing plugin inventory…");
        self.show_catalog("", out)?;
        self.show_installed("", out)?;
        self.request_core(out)
    }

    fn show_catalog<W: Write>(&mut self, query: &str, out: &mut W) -> io::Result<()> {
        let Some(paths) = &self.paths else {
            return self.list_error(CATALOG, "Catalog unavailable.", out);
        };
        let path = paths.config.join("catalog");
        let entries = match read_capped(&path, MAX_CATALOG_BYTES)
            .ok()
            .and_then(|b| plugin_manager::parse_catalog(&b).ok())
        {
            Some(v) => v,
            None => {
                return self.list_error(CATALOG, "Catalog unavailable — run plugins-refresh.", out)
            }
        };
        let mut rows = vec!["Local catalog".to_owned()];
        let mut ids = vec![None];
        for e in entries.into_iter().filter(|e| contains(&e.id, query)) {
            let Some(target) = catalog_target(&e, Action::Install) else {
                continue;
            };
            rows.extend([
                format!("  {} — {}", e.id, e.id),
                format!("    version: pinned"),
                format!("    repository: {}", e.repository),
                format!("    commit: {}", e.commit),
                "    select for install or enable".into(),
            ]);
            ids.extend([Some(target_token(&target)), None, None, None, None]);
        }
        if rows.len() == 1 {
            rows.push("  No catalog entries.".into());
            ids.push(None);
        }
        let truncated = self.put(CATALOG, rows, ids, out)?;
        if truncated {
            self.notice(out, "Catalog truncated; use a narrower query.");
        }
        Ok(())
    }

    fn show_installed<W: Write>(&mut self, query: &str, out: &mut W) -> io::Result<()> {
        let Some(paths) = &self.paths else {
            return self.list_error(INSTALLED, "Installed inventory unavailable.", out);
        };
        let mut rows = vec!["Installed plugins".into()];
        let mut ids = vec![None];
        let mut budget = InstalledBudget::new();
        let _ = budget.take(1); // heading row
        let dirs = fs::read_dir(&paths.data);
        if let Ok(dirs) = dirs {
            let mut id_count = 0;
            'ids: for id_dir in dirs {
                if !budget.take(1) {
                    break;
                }
                if id_count == MAX_DIRECTORY_ENTRIES {
                    budget.truncated = true;
                    break;
                }
                let Ok(id_dir) = id_dir else { continue };
                let Ok(meta) = fs::symlink_metadata(id_dir.path()) else {
                    continue;
                };
                if !real_directory(&meta) {
                    continue;
                }
                id_count += 1;
                let id = id_dir.file_name().to_string_lossy().into_owned();
                if plugin_manager::validate_plugin_id(&id).is_err() || !contains(&id, query) {
                    continue;
                }
                let Ok(versions) = fs::read_dir(id_dir.path()) else {
                    continue;
                };
                for (version_count, version) in versions.enumerate() {
                    if !budget.take(1) {
                        break 'ids;
                    }
                    if version_count == MAX_VERSION_ENTRIES {
                        budget.truncated = true;
                        break 'ids;
                    }
                    let Ok(version) = version else { continue };
                    let Ok(version_meta) = fs::symlink_metadata(version.path()) else {
                        continue;
                    };
                    if !real_directory(&version_meta) {
                        continue;
                    }
                    let version_name = version.file_name().to_string_lossy().into_owned();
                    if plugin_manager::validate_commit(&version_name).is_err() {
                        continue;
                    }
                    let receipt_path = version.path().join("receipt");
                    let Ok(receipt_meta) = fs::symlink_metadata(&receipt_path) else {
                        continue;
                    };
                    if !receipt_meta.is_file() || receipt_meta.file_type().is_symlink() {
                        continue;
                    }
                    if let Ok(r) = read_receipt(&receipt_path) {
                        if r.id != id || r.commit != version_name {
                            continue;
                        }
                        let Some(target) = receipt_target(&r, Action::Enable) else {
                            continue;
                        };
                        if rows.len().saturating_add(INSTALLED_ROWS_PER_RECEIPT) > MAX_WIDGET_ROWS
                            || !budget.take(1 + INSTALLED_ROWS_PER_RECEIPT)
                        {
                            budget.truncated = true;
                            break 'ids;
                        }
                        rows.extend([
                            format!("  {} — {}", r.id, r.id),
                            format!("    version: pinned"),
                            format!("    commit: {}", r.commit),
                            format!("    path: {}", version.path().join(&r.executable).display()),
                            "    desired: unknown".into(),
                            "    runtime: unknown".into(),
                        ]);
                        ids.extend([Some(target_token(&target)), None, None, None, None, None]);
                    }
                }
            }
        } else {
            return self.list_error(INSTALLED, "Installed inventory unavailable.", out);
        }
        if rows.len() == 1 {
            rows.push("  No plugins installed.".into());
            ids.push(None);
        }
        if budget.truncated {
            self.notice(out, "Installed inventory truncated; use a narrower query.");
        }
        let truncated = self.put(INSTALLED, rows, ids, out)?;
        if truncated {
            self.notice(out, "Installed inventory truncated; use a narrower query.");
        }
        Ok(())
    }

    fn show_core<W: Write>(&mut self, query: &str, out: &mut W) -> io::Result<()> {
        let mut rows = vec!["Core plugin state".into()];
        let mut ids = vec![None];
        for r in self
            .core_records
            .iter()
            .filter(|r| r.id != "teddy.manager" && contains(&r.id, query))
        {
            rows.extend([
                format!("  {}", r.id),
                format!("    path: {}", r.path),
                format!(
                    "    desired: {}",
                    if r.enabled { "enabled" } else { "disabled" }
                ),
                format!("    runtime: {}", state(r.state)),
                format!("    restart limit: {}", r.max_restarts),
                format!("    backoff: {} ms", r.backoff_ms),
            ]);
            ids.extend([
                self.target_for_id(&r.id).map(|t| target_token(&t)),
                None,
                None,
                None,
                None,
                None,
            ]);
        }
        if rows.len() == 1 {
            rows.push("  No core plugin records.".into());
            ids.push(None);
        }
        let truncated = self.put(CORE, rows, ids, out)?;
        if truncated {
            self.notice(out, "Core state truncated; use a narrower query.");
        }
        Ok(())
    }

    fn request_core<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.pending_lists.clear();
        let _ = self.put(
            CORE,
            vec![
                "Core plugin state".into(),
                "  Loading core plugin state…".into(),
            ],
            vec![None, None],
            out,
        )?;
        let id = loop {
            let candidate = self.next_request;
            self.next_request = self.next_request.wrapping_add(1).max(1);
            if !self.pending_lists.contains_key(&candidate) {
                break candidate;
            }
        };
        self.pending_lists.insert(
            id,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Core,
            },
        );
        send(
            out,
            encode_launcher_request(id, &LauncherRequest::List { page: 0 }).map_err(proto_io)?,
        )
    }

    fn launcher_response<W: Write>(&mut self, frame: Frame, out: &mut W) -> io::Result<()> {
        let response = match decode_launcher_response(&frame) {
            Ok(response) => response,
            Err(_) => {
                self.pending_lists.remove(&frame.request_id);
                self.controls.remove(&frame.request_id);
                self.notice(out, "Core state response was invalid; refresh again.");
                return Ok(());
            }
        };
        if let Some(control) = self.controls.remove(&frame.request_id) {
            let expected = &control.id;
            match response {
                LauncherResponse::Enabled(id) | LauncherResponse::Disabled(id)
                    if id == *expected =>
                {
                    self.start_poll(frame.request_id, control, out)?;
                }
                LauncherResponse::Forgotten(id) if id == *expected => {
                    if control.action == Action::Update && self.active_update.is_some() {
                        self.update_old_forgotten(out)?
                    } else {
                        self.finish_remove(control, out)
                    }
                }
                LauncherResponse::Enabled(_)
                | LauncherResponse::Disabled(_)
                | LauncherResponse::Forgotten(_) => self.notice(
                    out,
                    "Launcher response targeted a different plugin; nothing changed.",
                ),
                LauncherResponse::Error(_) => {
                    if control.action == Action::Update {
                        if self.active_update.as_ref().is_some_and(|a| {
                            matches!(
                                a.transaction.stage,
                                UpdateStage::CandidateEnableRequested
                                    | UpdateStage::RollbackRequested
                            )
                        }) {
                            let candidate_stage = self.active_update.as_ref().is_some_and(|a| {
                                a.transaction.stage == UpdateStage::CandidateEnableRequested
                            });
                            if candidate_stage {
                                if self.advance_active(UpdateStage::RollbackRequested, out)? {
                                    self.request_update_poll(
                                        control.target,
                                        Action::Disable,
                                        0,
                                        out,
                                    )?;
                                }
                            } else {
                                self.request_update_poll(control.target, Action::Disable, 0, out)?;
                            }
                        } else {
                            self.begin_update_rollback(out)?
                        }
                    } else {
                        self.notice(out, "Launcher rejected the lifecycle action.")
                    }
                }
                _ => self.notice(out, "Launcher returned an invalid lifecycle response."),
            }
            return Ok(());
        }
        let LauncherResponse::List {
            page,
            next_page,
            records,
        } = response
        else {
            self.pending_lists.remove(&frame.request_id);
            self.notice(out, "Core state request failed; refresh again.");
            return Ok(());
        };
        let next_to_send = {
            let Some(pending) = self.pending_lists.get_mut(&frame.request_id) else {
                self.notice(out, "Ignored an unknown core state response.");
                return Ok(());
            };
            if page != pending.expected_page
                || pending.pages >= MAX_LIST_PAGES
                || records.len() > MAX_LIST_PAGE_RECORDS
                || pending.records.saturating_add(records.len()) > MAX_LIST_RECORDS
            {
                self.pending_lists.remove(&frame.request_id);
                self.notice(out, "Core state pagination was invalid; refresh again.");
                return Ok(());
            }
            pending.pages += 1;
            pending.records += records.len();
            pending.collected.extend(records);
            if let Some(next) = next_page {
                if next <= page {
                    self.pending_lists.remove(&frame.request_id);
                    self.notice(out, "Core state pagination looped; refresh again.");
                    return Ok(());
                }
                pending.expected_page = next;
                Some(next)
            } else {
                None
            }
        };
        if let Some(next) = next_to_send {
            send(
                out,
                encode_launcher_request(frame.request_id, &LauncherRequest::List { page: next })
                    .map_err(proto_io)?,
            )?;
        } else {
            let purpose = self
                .pending_lists
                .get(&frame.request_id)
                .map(|p| p.purpose.clone())
                .unwrap_or(ListPurpose::Core);
            let records = self
                .pending_lists
                .remove(&frame.request_id)
                .map(|p| p.collected)
                .unwrap_or_default();
            self.core_records = records.clone();
            let mut rows = vec!["Core plugin state".into()];
            let mut ids = vec![None];
            for r in records.iter().filter(|r| r.id != "teddy.manager") {
                rows.extend([
                    format!("  {}", r.id),
                    format!("    path: {}", r.path),
                    format!(
                        "    desired: {}",
                        if r.enabled { "enabled" } else { "disabled" }
                    ),
                    format!("    runtime: {}", state(r.state)),
                    format!("    restart limit: {}", r.max_restarts),
                    format!("    backoff: {} ms", r.backoff_ms),
                ]);
                ids.extend([
                    self.target_for_id(&r.id).map(|t| target_token(&t)),
                    None,
                    None,
                    None,
                    None,
                    None,
                ]);
            }
            if rows.len() == 1 {
                rows.push("  No core plugin records.".into());
                ids.push(None);
            }
            let truncated = self.put(CORE, rows, ids, out)?;
            if truncated {
                self.notice(out, "Core state truncated; use a narrower query.");
            }
            self.notice(out, "Core state loaded.");
            if let ListPurpose::Lifecycle { target, poll, .. } = purpose.clone() {
                self.lifecycle_observed(target, poll, &records, out)?;
            }
            if matches!(purpose, ListPurpose::Core) {
                self.reconcile_pending_update(&records, out)?;
            }
        }
        Ok(())
    }

    fn launcher_event<W: Write>(&mut self, frame: Frame, out: &mut W) -> io::Result<()> {
        let Ok(event) = decode_launcher_event(&frame) else {
            self.notice(out, "Ignored an invalid launcher event.");
            return Ok(());
        };
        let id = match event {
            plugin::LauncherEvent::Enabled(id)
            | plugin::LauncherEvent::Disabled(id)
            | plugin::LauncherEvent::Reloaded(id)
            | plugin::LauncherEvent::Forgotten(id) => id,
        };
        if !self.controls.values().any(|c| c.id == id) {
            self.notice(out, "Ignored a launcher event for a different plugin.");
        }
        // Events are advisory. The correlated response owns the transition;
        // an event may arrive before that response.
        Ok(())
    }

    fn confirm<W: Write>(
        &mut self,
        revision: u64,
        index: usize,
        token: Option<&str>,
        out: &mut W,
    ) -> io::Result<()> {
        let Some(c) = self.confirmations.take() else {
            self.notice(out, "Confirmation expired; nothing changed.");
            return Ok(());
        };
        if index == 0 {
            if token == Some(confirmation_cancel_token(&c).as_str()) {
                return Ok(());
            }
            self.notice(out, "Confirmation changed; nothing was changed.");
            return Ok(());
        }
        let valid = revision == self.resources[&CONFIRM].revision
            && index == 1
            && token == Some(confirmation_token(&c).as_str())
            && self.resources[&c.source].revision == c.source_revision
            && self.valid_action_target(&c);
        if !valid {
            self.notice(out, "Selection changed; nothing was changed.");
            return Ok(());
        }
        match c.target.action {
            Action::Install | Action::InstallEnable => self.install(c, out),
            Action::Update => self.start_update(c, out),
            Action::Remove => {
                let id = c.target.id.clone();
                if self.record_for(&id).is_some() && !self.record_matches_target(&c.target) {
                    self.notice(
                        out,
                        "The launcher record does not match the selected payload; nothing changed.",
                    );
                    return Ok(());
                }
                if self.record_for(&id).is_none() {
                    if self.prepare_mutation() {
                        self.remove_exact(&c.target, out);
                    } else {
                        self.notice(
                            out,
                            "Manager recovery or lock could not be completed; nothing changed.",
                        );
                    }
                    return Ok(());
                }
                if self
                    .record_for(&id)
                    .is_some_and(|r| !r.enabled && r.state == 4)
                {
                    self.send_control(
                        Control {
                            id: id.clone(),
                            action: Action::Remove,
                            target: c.target.clone(),
                        },
                        LauncherRequest::Forget(id),
                        out,
                    )
                } else {
                    self.send_control(
                        Control {
                            id: id.clone(),
                            action: Action::Remove,
                            target: c.target.clone(),
                        },
                        LauncherRequest::Disable(id),
                        out,
                    )
                }
            }
            Action::Enable => {
                let id = c.target.id.clone();
                let Some(request) = self.enable_request(&c.target) else {
                    self.notice(
                        out,
                        "The selected payload or launcher record changed; nothing changed.",
                    );
                    return Ok(());
                };
                self.send_control(
                    Control {
                        id: id.clone(),
                        action: c.target.action,
                        target: c.target.clone(),
                    },
                    request,
                    out,
                )
            }
            Action::Disable => {
                let id = c.target.id.clone();
                if self.record_for(&id).is_some() && !self.record_matches_target(&c.target) {
                    self.notice(
                        out,
                        "The launcher record does not match the selected payload; nothing changed.",
                    );
                    return Ok(());
                }
                self.send_control(
                    Control {
                        id: id.clone(),
                        action: c.target.action,
                        target: c.target.clone(),
                    },
                    LauncherRequest::Disable(id),
                    out,
                )
            }
        }
    }

    fn start_update<W: Write>(&mut self, c: Confirmation, out: &mut W) -> io::Result<()> {
        let Some(old) = c.update_old else {
            self.notice(out, "Update target is incomplete; nothing changed.");
            return Ok(());
        };
        let Some(record) = self.record_for(&old.id) else {
            self.notice(out, "Update requires an observed enabled launcher record.");
            return Ok(());
        };
        let record_id = record.id.clone();
        if !record.enabled || !self.record_matches_target(&old) {
            self.notice(
                out,
                "Update requires the exact enabled payload; nothing changed.",
            );
            return Ok(());
        }
        let Some(paths) = &self.paths else {
            self.notice(out, "Manager storage is unavailable; nothing changed.");
            return Ok(());
        };
        let TargetSource::Catalog {
            repository,
            executable,
            platform,
        } = &c.target.source
        else {
            self.notice(
                out,
                "Update candidate source is unavailable; nothing changed.",
            );
            return Ok(());
        };
        let lock = match ManagerLock::acquire_for(paths) {
            Ok(lock) => lock,
            Err(_) => {
                self.notice(out, "Manager is busy; try again.");
                return Ok(());
            }
        };
        let manifest = PluginManifest {
            id: c.target.id.clone(),
            repository: repository.clone(),
            commit: c.target.commit.clone(),
            executables: [(platform.clone(), std::path::PathBuf::from(executable))]
                .into_iter()
                .collect(),
        };
        let Ok(payload) = install_git(paths, &lock, &manifest, "manifest", platform) else {
            self.notice(out, "Update candidate could not be installed safely.");
            return Ok(());
        };
        let Some(receipt) = read_receipt(&payload.parent().unwrap().join("receipt")).ok() else {
            self.notice(out, "Update candidate receipt could not be verified.");
            return Ok(());
        };
        let candidate_identity = UpdateIdentity {
            receipt: receipt.clone(),
            payload: payload.clone(),
        };
        let Some(old_identity) = self.update_identity(paths, &old) else {
            self.notice(
                out,
                "Old payload identity could not be verified; nothing changed.",
            );
            return Ok(());
        };
        let Ok(transaction) = begin_update(paths, &lock, old_identity, candidate_identity) else {
            self.notice(
                out,
                "An update is already pending or could not be recorded.",
            );
            return Ok(());
        };
        drop(lock);
        let mut candidate = c.target.clone();
        candidate.receipt = receipt_target(&receipt, Action::Update).and_then(|t| t.receipt);
        self.active_update = Some(ActiveUpdate {
            transaction,
            old: old.clone(),
            candidate: candidate.clone(),
        });
        self.send_control(
            Control {
                id: old.id.clone(),
                action: Action::Update,
                target: old,
            },
            LauncherRequest::Disable(record_id),
            out,
        )
    }

    fn update_identity(&self, paths: &ManagerPaths, target: &Target) -> Option<UpdateIdentity> {
        let ReceiptIdentity { executable, digest } = target.receipt.as_ref()?;
        let payload = plugin_manager::payload_path(paths, &target.id, &target.commit).ok()?;
        let receipt = read_receipt(&payload.parent()?.join("receipt")).ok()?;
        (receipt.id == target.id
            && receipt.commit == target.commit
            && receipt.executable.to_string_lossy() == executable.as_str()
            && receipt.digest.as_deref() == Some(digest.as_str())
            && payload_digest(&payload).ok()?.as_str() == digest)
            .then_some(UpdateIdentity { receipt, payload })
    }

    fn install<W: Write>(&mut self, c: Confirmation, out: &mut W) -> io::Result<()> {
        let Some(paths) = &self.paths else {
            self.notice(out, "Manager storage is unavailable; nothing changed.");
            return Ok(());
        };
        let TargetSource::Catalog {
            repository,
            executable,
            platform,
        } = &c.target.source
        else {
            self.notice(
                out,
                "Install target is not a catalog entry; nothing was installed.",
            );
            return Ok(());
        };
        let lock = match ManagerLock::acquire_for(paths) {
            Ok(l) => l,
            Err(_) => {
                self.notice(out, "Manager is busy; try again.");
                return Ok(());
            }
        };
        let manifest = PluginManifest {
            id: c.target.id.clone(),
            repository: repository.clone(),
            commit: c.target.commit.clone(),
            executables: [(platform.clone(), std::path::PathBuf::from(executable))]
                .into_iter()
                .collect(),
        };
        let result = install_git(paths, &lock, &manifest, "manifest", platform);
        drop(lock);
        let Ok(payload) = result else {
            self.notice(out, "Install could not be completed safely.");
            return Ok(());
        };
        let Some(installed_receipt) = read_receipt(&payload.parent().unwrap().join("receipt"))
            .ok()
            .and_then(|r| receipt_target(&r, Action::InstallEnable).map(|t| t.receipt.unwrap()))
        else {
            self.notice(
                out,
                "Installed payload identity could not be verified; nothing was enabled.",
            );
            return Ok(());
        };
        if c.target.action == Action::Install {
            self.notice(out, "Installed; plugin remains disabled.");
            return Ok(());
        }
        let mut installed_target = c.target.clone();
        installed_target.receipt = Some(installed_receipt);
        let request = if self.record_for(&installed_target.id).is_some() {
            let Some(request) = self.enable_request(&installed_target) else {
                self.notice(out, "The launcher record does not match the installed payload; nothing was enabled.");
                return Ok(());
            };
            request
        } else {
            LauncherRequest::Enable {
                id: installed_target.id.clone(),
                descriptor: Some(LauncherDescriptor {
                    path: payload.display().to_string(),
                    max_restarts: 3,
                    backoff_ms: 250,
                    confirm_recovery: false,
                }),
            }
        };
        self.send_control(
            Control {
                id: installed_target.id.clone(),
                action: Action::InstallEnable,
                target: installed_target,
            },
            request,
            out,
        )
    }

    fn send_control<W: Write>(
        &mut self,
        control: Control,
        request: LauncherRequest,
        out: &mut W,
    ) -> io::Result<()> {
        if !self.prepare_mutation() {
            self.notice(
                out,
                "Manager recovery or lock could not be completed; nothing changed.",
            );
            return Ok(());
        }
        let id = self.next_request_id();
        self.controls.insert(id, control);
        send(
            out,
            encode_launcher_request(id, &request).map_err(proto_io)?,
        )
    }

    fn start_poll<W: Write>(&mut self, id: u32, control: Control, out: &mut W) -> io::Result<()> {
        let mut poll_target = control.target.clone();
        if control.action == Action::Update {
            if let Some(active) = &self.active_update {
                poll_target.action = match active.transaction.stage {
                    UpdateStage::CandidateEnableRequested | UpdateStage::RollbackRunning => {
                        Action::Enable
                    }
                    _ => Action::Disable,
                };
            }
        }
        self.pending_lists.insert(
            id,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Lifecycle {
                    target: poll_target,
                    poll: 0,
                },
            },
        );
        send(
            out,
            encode_launcher_request(id, &LauncherRequest::List { page: 0 }).map_err(proto_io)?,
        )
    }

    fn lifecycle_observed<W: Write>(
        &mut self,
        target: Target,
        poll: usize,
        records: &[plugin::LauncherRecord],
        out: &mut W,
    ) -> io::Result<()> {
        let id = target.id.clone();
        let action = target.action;
        let record = records.iter().find(|r| r.id == id);
        if record.is_some()
            && !self.record_path_matches_target(&target, record.unwrap().path.as_str())
        {
            self.notice(
                out,
                "The observed launcher record does not match the selected payload.",
            );
            return Ok(());
        }
        let failed = record.is_some_and(|r| r.state == 4);
        if let Some(active) = self.active_update.clone() {
            if target.commit == active.candidate.commit
                && (failed || poll + 1 >= MAX_STATE_POLLS)
                && target.action == Action::Enable
            {
                return self.begin_update_rollback(out);
            }
            if target.commit == active.old.commit
                && target.action == Action::Disable
                && (record.is_none()
                    || records
                        .iter()
                        .any(|r| r.id == id && !r.enabled && r.state == 4))
            {
                if !self.advance_active(UpdateStage::OldQuiesced, out)? {
                    return Ok(());
                }
                self.send_control(
                    Control {
                        id: id.clone(),
                        action: Action::Update,
                        target: active.old.clone(),
                    },
                    LauncherRequest::Forget(id),
                    out,
                )?;
                return Ok(());
            }
            if target.commit == active.candidate.commit
                && target.action == Action::Enable
                && records
                    .iter()
                    .any(|r| r.id == id && r.enabled && r.state == 1)
            {
                if active.transaction.stage == UpdateStage::CandidateEnableRequested
                    && !self.advance_active(UpdateStage::CandidateRunning, out)?
                {
                    return Ok(());
                }
                if !self.advance_active(UpdateStage::Completed, out)? {
                    return Ok(());
                }
                self.active_update = None;
                self.notice(out, "Plugin update completed.");
                return Ok(());
            }
            if target.commit == active.candidate.commit
                && target.action == Action::Disable
                && (record.is_none() || failed)
            {
                if !self.advance_active(UpdateStage::RollbackRunning, out)? {
                    return Ok(());
                }
                let old = active.old.clone();
                let Some(descriptor) = self.enable_descriptor(&old) else {
                    self.notice(
                        out,
                        "Rollback payload could not be verified; update remains pending.",
                    );
                    return Ok(());
                };
                let request = LauncherRequest::Enable {
                    id: old.id.clone(),
                    descriptor: Some(descriptor),
                };
                self.send_control(
                    Control {
                        id: old.id.clone(),
                        action: Action::Update,
                        target: old,
                    },
                    request,
                    out,
                )?;
                return Ok(());
            }
            if target.commit == active.old.commit
                && target.action == Action::Enable
                && records
                    .iter()
                    .any(|r| r.id == id && r.enabled && r.state == 1)
                && active.transaction.stage == UpdateStage::RollbackRunning
            {
                if !self.advance_active(UpdateStage::Completed, out)? {
                    return Ok(());
                }
                self.active_update = None;
                self.notice(out, "Plugin update rolled back safely.");
                return Ok(());
            }
            if target.commit == active.old.commit || target.commit == active.candidate.commit {
                return self.request_update_poll(target, action, poll + 1, out);
            }
        }
        let ready = match action {
            Action::Enable | Action::InstallEnable => records
                .iter()
                .any(|r| r.id == id && r.enabled && r.state == 1),
            Action::Disable => {
                record.is_none()
                    || records
                        .iter()
                        .any(|r| r.id == id && !r.enabled && r.state != 1)
            }
            Action::Remove => {
                record.is_none()
                    || records
                        .iter()
                        .any(|r| r.id == id && !r.enabled && r.state == 4)
            }
            _ => false,
        };
        if failed && action != Action::Remove {
            self.notice(out, "Plugin failed to reach the requested state.");
            return Ok(());
        }
        if ready {
            match action {
                Action::Remove if record.is_none() => {
                    self.remove_exact(&target, out);
                }
                Action::Remove => {
                    self.send_control(
                        Control {
                            id: id.clone(),
                            action,
                            target: target.clone(),
                        },
                        LauncherRequest::Forget(id),
                        out,
                    )?;
                }
                _ => self.notice(out, "Plugin state confirmed."),
            }
            return Ok(());
        }
        if poll + 1 >= MAX_STATE_POLLS {
            self.notice(
                out,
                "Plugin state did not settle before the bounded wait ended.",
            );
            return Ok(());
        }
        let request = self.next_request_id();
        self.pending_lists.insert(
            request,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Lifecycle {
                    target,
                    poll: poll + 1,
                },
            },
        );
        send(
            out,
            encode_launcher_request(request, &LauncherRequest::List { page: 0 })
                .map_err(proto_io)?,
        )
    }

    fn request_update_poll<W: Write>(
        &mut self,
        mut target: Target,
        action: Action,
        poll: usize,
        out: &mut W,
    ) -> io::Result<()> {
        if poll >= MAX_STATE_POLLS {
            self.notice(
                out,
                "Update state did not settle before the bounded wait ended; journal retained.",
            );
            return Ok(());
        }
        target.action = action;
        let request = self.next_request_id();
        self.pending_lists.insert(
            request,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Lifecycle { target, poll },
            },
        );
        send(
            out,
            encode_launcher_request(request, &LauncherRequest::List { page: 0 })
                .map_err(proto_io)?,
        )
    }

    fn prepare_mutation(&self) -> bool {
        let Some(paths) = &self.paths else {
            return false;
        };
        let Ok(lock) = ManagerLock::acquire_for(paths) else {
            return false;
        };
        let ok = recover_manager(paths).is_ok();
        drop(lock);
        ok
    }

    fn advance_active<W: Write>(&mut self, next: UpdateStage, out: &mut W) -> io::Result<bool> {
        let Some(active) = self.active_update.clone() else {
            return Ok(false);
        };
        let Some(paths) = &self.paths else {
            self.notice(
                out,
                "Update journal was retained; manager storage is unavailable.",
            );
            return Ok(false);
        };
        let Ok(lock) = ManagerLock::acquire_for(paths) else {
            self.notice(out, "Update journal was retained; manager storage is busy.");
            return Ok(false);
        };
        let Ok(transaction) = advance_update(paths, &lock, &active.transaction, next) else {
            self.notice(
                out,
                "Update journal was retained; its stage could not be advanced safely.",
            );
            return Ok(false);
        };
        if let Some(active) = &mut self.active_update {
            active.transaction = transaction;
        }
        Ok(true)
    }
    fn update_old_forgotten<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        let Some(active) = self.active_update.clone() else {
            return Ok(());
        };
        let candidate = active.candidate.clone();
        if !self.advance_active(UpdateStage::CandidateEnableRequested, out)? {
            return Ok(());
        }
        let Some(descriptor) = self.enable_descriptor(&candidate) else {
            self.notice(
                out,
                "Candidate payload could not be verified; update journal was retained.",
            );
            return Ok(());
        };
        let request = LauncherRequest::Enable {
            id: candidate.id.clone(),
            descriptor: Some(descriptor),
        };
        self.send_control(
            Control {
                id: candidate.id.clone(),
                action: Action::Update,
                target: candidate,
            },
            request,
            out,
        )
    }
    fn begin_update_rollback<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        let Some(active) = self.active_update.clone() else {
            return Ok(());
        };
        if !matches!(
            active.transaction.stage,
            UpdateStage::OldQuiesced
                | UpdateStage::CandidateEnableRequested
                | UpdateStage::CandidateRunning
        ) {
            self.notice(
                out,
                "Update remains pending; rollback is already in progress.",
            );
            return Ok(());
        }
        if !self.advance_active(UpdateStage::RollbackRequested, out)? {
            return Ok(());
        }
        let candidate = active.candidate.clone();
        self.send_control(
            Control {
                id: candidate.id.clone(),
                action: Action::Update,
                target: candidate,
            },
            LauncherRequest::Disable(active.candidate.id.clone()),
            out,
        )
    }
    fn reconcile_pending_update<W: Write>(
        &mut self,
        records: &[plugin::LauncherRecord],
        out: &mut W,
    ) -> io::Result<()> {
        if self.active_update.is_some() {
            return Ok(());
        }
        let Some(paths) = &self.paths else {
            return Ok(());
        };
        let Ok(Some(transaction)) = pending_update(paths) else {
            return Ok(());
        };
        let (Some(old), Some(candidate)) = (
            receipt_target(&transaction.old.receipt, Action::Enable),
            receipt_target(&transaction.candidate.receipt, Action::Enable),
        ) else {
            self.notice(
                out,
                "A pending update has an invalid payload identity; its journal was retained.",
            );
            return Ok(());
        };
        self.active_update = Some(ActiveUpdate {
            transaction,
            old,
            candidate,
        });
        let active = self.active_update.clone().unwrap();
        let candidate_record = records.iter().find(|r| r.id == active.candidate.id);
        let candidate_running = candidate_record.is_some_and(|r| {
            r.enabled && r.state == 1 && self.record_path_matches_target(&active.candidate, &r.path)
        });
        let old_record = records.iter().find(|r| r.id == active.old.id);
        let old_running = old_record
            .is_some_and(|r| r.enabled && self.record_path_matches_target(&active.old, &r.path));
        match active.transaction.stage {
            UpdateStage::CandidateInstalled => {
                if candidate_running {
                    if !self.advance_active(UpdateStage::CandidateRunning, out)? {
                        return Ok(());
                    }
                    if !self.advance_active(UpdateStage::Completed, out)? {
                        return Ok(());
                    }
                    self.active_update = None;
                    self.notice(out, "Pending update completed after recovery.");
                } else if old_running {
                    self.send_control(
                        Control {
                            id: active.old.id.clone(),
                            action: Action::Update,
                            target: active.old.clone(),
                        },
                        LauncherRequest::Disable(active.old.id),
                        out,
                    )?;
                } else if old_record.is_none()
                    || old_record.is_some_and(|r| !r.enabled && r.state == 4)
                {
                    if !self.advance_active(UpdateStage::OldQuiesced, out)? {
                        return Ok(());
                    }
                    if old_record.is_some() {
                        self.send_control(
                            Control {
                                id: active.old.id.clone(),
                                action: Action::Update,
                                target: active.old.clone(),
                            },
                            LauncherRequest::Forget(active.old.id),
                            out,
                        )?;
                    } else {
                        self.update_old_forgotten(out)?;
                    }
                } else {
                    self.request_update_poll(active.old, Action::Disable, 0, out)?;
                }
            }
            UpdateStage::OldQuiesced => {
                if candidate_running {
                    if !self.advance_active(UpdateStage::CandidateEnableRequested, out)? {
                        return Ok(());
                    };
                    if !self.advance_active(UpdateStage::CandidateRunning, out)? {
                        return Ok(());
                    };
                    if !self.advance_active(UpdateStage::Completed, out)? {
                        return Ok(());
                    };
                    self.active_update = None;
                    self.notice(out, "Pending update completed after recovery.");
                } else {
                    self.update_old_forgotten(out)?;
                }
            }
            UpdateStage::CandidateEnableRequested => {
                self.request_update_poll(active.candidate, Action::Enable, 0, out)?
            }
            UpdateStage::CandidateRunning => {
                if candidate_running {
                    if !self.advance_active(UpdateStage::Completed, out)? {
                        return Ok(());
                    }
                    self.active_update = None;
                    self.notice(out, "Pending update completed after recovery.");
                } else if candidate_record.is_none()
                    || candidate_record.is_some_and(|r| r.state == 4)
                {
                    self.begin_update_rollback(out)?;
                } else {
                    self.request_update_poll(active.candidate, Action::Enable, 0, out)?;
                }
            }
            UpdateStage::RollbackRequested => {
                self.request_update_poll(active.candidate, Action::Disable, 0, out)?
            }
            UpdateStage::RollbackRunning => {
                self.request_update_poll(active.old, Action::Enable, 0, out)?
            }
            UpdateStage::Completed | UpdateStage::Aborted => self.notice(
                out,
                "A terminal update journal was retained for safe inspection.",
            ),
        }
        Ok(())
    }

    fn finish_remove<W: Write>(&mut self, c: Control, out: &mut W) {
        self.remove_exact(&c.target, out);
    }
    fn remove_exact<W: Write>(&mut self, target: &Target, out: &mut W) {
        let Some(paths) = &self.paths else {
            self.notice(
                out,
                "Plugin was disabled but storage is unavailable; it was not removed.",
            );
            return;
        };
        let Some(ReceiptIdentity { executable, digest }) = &target.receipt else {
            self.notice(
                out,
                "The selected installed receipt was unavailable; it was not removed.",
            );
            return;
        };
        let Ok(lock) = ManagerLock::acquire_for(paths) else {
            self.notice(
                out,
                "Plugin was disabled; manager storage is busy, so it was not removed.",
            );
            return;
        };
        if recover_manager(paths).is_err() {
            self.notice(
                out,
                "Manager recovery could not be completed; it was not removed.",
            );
            return;
        }
        let Ok(payload) = plugin_manager::payload_path(paths, &target.id, &target.commit)
            .map(|p| p.parent().unwrap().to_path_buf())
        else {
            self.notice(out, "Plugin was disabled but removal was not safe.");
            return;
        };
        let expected = Receipt {
            id: target.id.clone(),
            commit: target.commit.clone(),
            executable: std::path::PathBuf::from(executable),
            digest: Some(digest.clone()),
        };
        if !payload.exists() {
            match plugin_manager::remove_installed(paths, &lock, &expected) {
                Ok(RemovalResult::NotFound) => self.notice(out, "Plugin was already absent."),
                Ok(RemovalResult::Removed) => self.notice(out, "Plugin removed."),
                Err(_) => self.notice(
                    out,
                    "Plugin was disabled but removal could not be completed safely.",
                ),
            }
            return;
        }
        let receipt = payload.join("receipt");
        let Ok(r) = read_receipt(&receipt) else {
            self.notice(
                out,
                "The selected receipt could not be verified; it was not removed.",
            );
            return;
        };
        let Ok(actual_digest) = payload_digest(&payload.join(executable)) else {
            self.notice(
                out,
                "The selected payload could not be verified; it was not removed.",
            );
            return;
        };
        if r.id != target.id
            || r.commit != target.commit
            || r.executable.to_string_lossy() != executable.as_str()
            || r.digest.as_deref() != Some(digest.as_str())
            || actual_digest != *digest
        {
            self.notice(
                out,
                "The selected installed version changed; it was not removed.",
            );
            return;
        }
        match plugin_manager::remove_installed(paths, &lock, &expected) {
            Ok(RemovalResult::Removed) => self.notice(out, "Plugin removed."),
            Ok(RemovalResult::NotFound) => self.notice(out, "Plugin was already absent."),
            Err(_) => self.notice(
                out,
                "Plugin was disabled but removal could not be completed safely.",
            ),
        }
        drop(lock);
    }

    fn next_request_id(&mut self) -> u32 {
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        id
    }
    fn catalog_entry(&self, id: &str) -> Option<CatalogEntry> {
        let paths = self.paths.as_ref()?;
        let bytes = read_capped(&paths.config.join("catalog"), MAX_CATALOG_BYTES).ok()?;
        plugin_manager::parse_catalog(&bytes)
            .ok()?
            .into_iter()
            .find(|e| e.id == id)
    }
    fn record_for(&self, id: &str) -> Option<&plugin::LauncherRecord> {
        self.core_records.iter().find(|r| r.id == id)
    }
    fn enable_descriptor(&self, target: &Target) -> Option<LauncherDescriptor> {
        let Some(ReceiptIdentity { executable, digest }) = &target.receipt else {
            return None;
        };
        let paths = self.paths.as_ref()?;
        let version = plugin_manager::payload_path(paths, &target.id, &target.commit)
            .ok()?
            .parent()?
            .to_path_buf();
        let receipt = read_receipt(&version.join("receipt")).ok()?;
        if receipt.id != target.id
            || receipt.commit != target.commit
            || receipt.executable.to_string_lossy() != executable.as_str()
            || receipt.digest.as_deref() != Some(digest.as_str())
        {
            return None;
        }
        let payload = version.join(executable);
        if payload_digest(&payload).ok()?.as_str() != digest {
            return None;
        }
        Some(LauncherDescriptor {
            path: payload.display().to_string(),
            max_restarts: 3,
            backoff_ms: 250,
            confirm_recovery: false,
        })
    }
    fn enable_request(&self, target: &Target) -> Option<LauncherRequest> {
        let descriptor = self.enable_descriptor(target)?;
        if let Some(record) = self.record_for(&target.id) {
            if record.path != descriptor.path {
                return None;
            }
            return Some(LauncherRequest::Enable {
                id: target.id.clone(),
                descriptor: None,
            });
        }
        Some(LauncherRequest::Enable {
            id: target.id.clone(),
            descriptor: Some(descriptor),
        })
    }
    fn record_matches_target(&self, target: &Target) -> bool {
        self.record_for(&target.id)
            .is_some_and(|r| self.record_path_matches_target(target, &r.path))
    }
    fn record_path_matches_target(&self, target: &Target, path: &str) -> bool {
        let Some(ReceiptIdentity { executable, digest }) = &target.receipt else {
            return false;
        };
        let Some(paths) = &self.paths else {
            return false;
        };
        let Ok(payload) = plugin_manager::payload_path(paths, &target.id, &target.commit) else {
            return false;
        };
        payload
            .parent()
            .map(|v| v.join(executable).display().to_string())
            .as_deref()
            == Some(path)
            && self.receipt_matches(target)
            && payload_digest(&payload.parent().unwrap().join(executable))
                .ok()
                .as_deref()
                == Some(digest.as_str())
    }
    fn valid_action_target(&self, c: &Confirmation) -> bool {
        c.target.id != MANAGER_ID
            && target_valid(&c.target)
            && match &c.target.source {
                TargetSource::Catalog { .. } => self.catalog_matches(&c.target),
                TargetSource::Receipt { .. } => self.receipt_matches(&c.target),
            }
            && c.update_old.as_ref().is_none_or(|old| {
                old.id == c.target.id && old.commit != c.target.commit && self.receipt_matches(old)
            })
    }
    fn catalog_matches(&self, target: &Target) -> bool {
        let Some(e) = self.catalog_entry(&target.id) else {
            return false;
        };
        catalog_target(&e, target.action).is_some_and(|now| {
            now.id == target.id
                && now.commit == target.commit
                && now.source == target.source
                && (target.receipt.is_none() || self.receipt_matches(target))
        })
    }
    fn receipt_matches(&self, target: &Target) -> bool {
        let Some(paths) = &self.paths else {
            return false;
        };
        let Ok(p) = plugin_manager::payload_path(paths, &target.id, &target.commit) else {
            return false;
        };
        let Ok(r) = read_receipt(&p.parent().unwrap().join("receipt")) else {
            return false;
        };
        match &target.receipt {
            Some(ReceiptIdentity { executable, digest }) => {
                r.id == target.id
                    && r.commit == target.commit
                    && r.executable.to_string_lossy() == executable.as_str()
                    && r.digest.as_deref() == Some(digest.as_str())
                    && payload_digest(&p.parent().unwrap().join(executable))
                        .ok()
                        .as_deref()
                        == Some(digest.as_str())
            }
            _ => false,
        }
    }
    fn is_installed_version(&self, target: &Target) -> bool {
        target.receipt.is_some() && self.receipt_matches(target)
    }
    fn target_for_id(&self, id: &str) -> Option<Target> {
        let paths = self.paths.as_ref()?;
        let mut found = None;
        if let Ok(versions) = fs::read_dir(paths.data.join(id)) {
            for v in versions.flatten() {
                if let Ok(r) = read_receipt(&v.path().join("receipt")) {
                    if r.id == id {
                        if found.is_some() {
                            return None;
                        }
                        found = receipt_target(&r, Action::Enable);
                    }
                }
            }
        }
        found.or_else(|| {
            self.catalog_entry(id)
                .and_then(|e| catalog_target(&e, Action::Enable))
        })
    }
    fn with_receipt(&self, target: &Target) -> Target {
        if target.receipt.is_some() {
            return target.clone();
        }
        let Some(paths) = &self.paths else {
            return target.clone();
        };
        let Ok(p) = plugin_manager::payload_path(paths, &target.id, &target.commit) else {
            return target.clone();
        };
        let Ok(r) = read_receipt(&p.parent().unwrap().join("receipt")) else {
            return target.clone();
        };
        let Some(rt) = receipt_target(&r, target.action) else {
            return target.clone();
        };
        let mut out = target.clone();
        out.receipt = rt.receipt;
        out
    }

    fn selection<W: Write>(&mut self, frame: Frame, out: &mut W) -> io::Result<()> {
        if frame.payload.len() != 4 {
            return Ok(());
        }
        let index = u32::from_le_bytes(frame.payload.try_into().unwrap()) as usize;
        if frame.resource_id == CONFIRM && index == 0 {
            let token = self.confirmations.as_ref().map(confirmation_cancel_token);
            return self.confirm(frame.resource_revision, index, token.as_deref(), out);
        }
        let Some(resource) = self.resources.get(&frame.resource_id) else {
            return Ok(());
        };
        let Some(id) = resource
            .valid_selection(frame.resource_revision, index)
            .map(str::to_owned)
        else {
            self.notice(out, "View changed; selection was not applied.");
            return Ok(());
        };
        if frame.resource_id == CONFIRM {
            let token = self
                .resources
                .get(&CONFIRM)
                .and_then(|r| r.valid_selection(frame.resource_revision, index))
                .map(str::to_owned);
            return self.confirm(frame.resource_revision, index, token.as_deref(), out);
        }
        if frame.resource_id == ACTIONS {
            let (mut target, update_old) = if let Some((old, candidate)) = parse_update_token(&id) {
                (candidate, Some(old))
            } else {
                let Some(target) = parse_target(&id) else {
                    return Ok(());
                };
                (target, None)
            };
            let action = target.action;
            let Some(source_resource) = self.resources.get(
                &self
                    .active
                    .map(|v| match v {
                        View::Catalog => CATALOG,
                        View::Installed => INSTALLED,
                        View::Core => CORE,
                    })
                    .unwrap_or(CATALOG),
            ) else {
                return Ok(());
            };
            if target.id == MANAGER_ID {
                self.notice(out, "The manager cannot be changed.");
                return Ok(());
            }
            let source = source_id(self.active);
            target.action = action;
            let mut confirm_rows = vec![
                "Cancel".into(),
                format!("Confirm {}", action_label(action)),
                format!("Plugin: {}", target.id),
            ];
            if let Some(old) = &update_old {
                confirm_rows.extend([
                    format!("Old commit: {}", old.commit),
                    format!("Candidate commit: {}", target.commit),
                    "Rollback restores the old payload if candidate startup fails.".into(),
                ]);
            } else {
                confirm_rows.extend([
                    format!("Source: {} @ {}", source_label(&target), target.commit),
                    consequence(action).into(),
                ]);
            }
            self.confirmations = Some(Confirmation {
                source,
                source_revision: source_resource.revision,
                target: target.clone(),
                update_old: update_old.clone(),
            });
            return self
                .put(
                    CONFIRM,
                    confirm_rows,
                    vec![
                        Some(confirmation_cancel_token(&Confirmation {
                            source,
                            source_revision: source_resource.revision,
                            target: target.clone(),
                            update_old: update_old.clone(),
                        })),
                        Some(confirmation_token(&Confirmation {
                            source,
                            source_revision: source_resource.revision,
                            target: target.clone(),
                            update_old: update_old.clone(),
                        })),
                        None,
                        None,
                        None,
                    ],
                    out,
                )
                .map(|_| ());
        }
        if frame.resource_id != CATALOG
            && frame.resource_id != INSTALLED
            && frame.resource_id != CORE
        {
            return Ok(());
        }
        let Some(base) = parse_target(&id) else {
            return Ok(());
        };
        if base.id == MANAGER_ID {
            return Ok(());
        }
        let installed_target = self.with_receipt(&base);
        let installed = self.is_installed_version(&installed_target);
        let mut rows = vec![
            format!("Actions for {}", base.id),
            "Select an action".into(),
        ];
        let mut action_ids = vec![None, None];
        let add = |rows: &mut Vec<String>, ids: &mut Vec<Option<String>>, a: Action| {
            rows.push(format!("  {}", action_label(a)));
            let mut target = if matches!(base.source, TargetSource::Catalog { .. })
                && matches!(a, Action::Enable | Action::Disable | Action::Remove)
            {
                installed_target.clone()
            } else {
                base.clone()
            };
            target.action = a;
            ids.push(Some(target_token(&target)));
        };
        if frame.resource_id == CATALOG && !installed {
            add(&mut rows, &mut action_ids, Action::Install);
            add(&mut rows, &mut action_ids, Action::InstallEnable);
        }
        if installed || frame.resource_id == CORE {
            let enabled = self
                .record_for(&base.id)
                .map(|r| r.enabled)
                .unwrap_or(false);
            add(
                &mut rows,
                &mut action_ids,
                if enabled {
                    Action::Disable
                } else {
                    Action::Enable
                },
            );
            if installed {
                add(&mut rows, &mut action_ids, Action::Remove);
                if self.record_for(&base.id).is_some_and(|r| r.enabled) {
                    if let Some(candidate) = self
                        .catalog_entry(&base.id)
                        .and_then(|e| catalog_target(&e, Action::Update))
                    {
                        if candidate.commit != installed_target.commit {
                            rows.push("  Update".into());
                            action_ids.push(Some(update_token(&installed_target, &candidate)));
                        }
                    }
                }
            }
        }
        self.put(ACTIONS, rows, action_ids, out).map(|_| ())
    }

    fn put<W: Write>(
        &mut self,
        id: u64,
        rows: Vec<String>,
        ids: Vec<Option<String>>,
        out: &mut W,
    ) -> io::Result<bool> {
        let r = self.resources.get_mut(&id).unwrap();
        let (rows, ids, payload, truncated) = build_widget(rows, ids);
        r.replace(rows, ids);
        send(
            out,
            Frame {
                msg_type: WIDGET,
                flags: 0,
                request_id: 0,
                resource_id: id,
                resource_revision: r.revision,
                payload,
            },
        )?;
        Ok(truncated)
    }
    fn list_error<W: Write>(&mut self, id: u64, text: &str, out: &mut W) -> io::Result<()> {
        self.put(id, vec![text.into()], vec![None], out).map(|_| ())
    }
    fn notice<W: Write>(&self, out: &mut W, text: &str) {
        let _ = send(out, status_frame(text));
    }
}

fn parse_command(s: &str) -> (&str, &str) {
    s.trim()
        .split_once(char::is_whitespace)
        .map_or((s.trim(), ""), |(a, b)| (a, b.trim()))
}
fn contains(value: &str, query: &str) -> bool {
    query.is_empty()
        || value
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
}
fn state(s: u8) -> &'static str {
    ["starting", "running", "stopping", "backoff", "failed"]
        .get(s as usize)
        .copied()
        .unwrap_or("failed")
}
fn source_id(view: Option<View>) -> u64 {
    match view {
        Some(View::Installed) => INSTALLED,
        Some(View::Core) => CORE,
        _ => CATALOG,
    }
}
fn action_label(a: Action) -> &'static str {
    match a {
        Action::Install => "Install (keeps disabled)",
        Action::InstallEnable => "Install and enable",
        Action::Enable => "Enable",
        Action::Disable => "Disable",
        Action::Remove => "Remove",
        Action::Update => "Update",
    }
}
fn consequence(a: Action) -> &'static str {
    match a {
        Action::Install => "Downloads the pinned source and leaves the plugin disabled.",
        Action::InstallEnable => "Downloads the pinned source, then waits for it to run.",
        Action::Enable => "Enables the plugin and waits for observed running state.",
        Action::Disable => "Disables the plugin and waits for observed stopped state.",
        Action::Remove => "Disables, waits for reaping, forgets, then removes local files.",
        Action::Update => {
            "Switches to the pinned candidate; rollback restores the old payload if startup fails."
        }
    }
}
fn source_label(t: &Target) -> String {
    match &t.source {
        TargetSource::Catalog { repository, .. } => repository.clone(),
        TargetSource::Receipt { .. } => "installed receipt".into(),
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|x| u8::from_str_radix(std::str::from_utf8(x).ok()?, 16).ok())
        .collect()
}
fn field(s: &str) -> String {
    hex(s.as_bytes())
}
fn unfield(s: &str) -> Option<String> {
    String::from_utf8(unhex(s)?).ok()
}
fn target_token(t: &Target) -> String {
    let mut v = format!(
        "target:{}:{}:{}",
        t.action as u8,
        field(&t.id),
        field(&t.commit)
    );
    match &t.source {
        TargetSource::Catalog {
            repository,
            executable,
            platform,
        } => v.push_str(&format!(
            ":c:{}:{}:{}",
            field(repository),
            field(executable),
            field(platform)
        )),
        TargetSource::Receipt { executable, digest } => {
            v.push_str(&format!(":r:{}:{}", field(executable), field(digest)))
        }
    }
    match &t.receipt {
        Some(r) => v.push_str(&format!(":d:{}:{}", field(&r.executable), field(&r.digest))),
        None => v.push_str(":n"),
    }
    v
}
fn parse_target(s: &str) -> Option<Target> {
    let p: Vec<_> = s.split(':').collect();
    if p.len() < 5 || p[0] != "target" {
        return None;
    }
    let action = match p[1].parse().ok()? {
        0 => Action::Install,
        1 => Action::InstallEnable,
        2 => Action::Enable,
        3 => Action::Disable,
        4 => Action::Remove,
        5 => Action::Update,
        _ => return None,
    };
    let id = unfield(p[2])?;
    let commit = unfield(p[3])?;
    plugin_manager::validate_plugin_id(&id).ok()?;
    plugin_manager::validate_commit(&commit).ok()?;
    let source = match p[4] {
        "c" if p.len() == 9 || p.len() == 11 => TargetSource::Catalog {
            repository: unfield(p[5])?,
            executable: unfield(p[6])?,
            platform: unfield(p[7])?,
        },
        "r" if p.len() == 8 || p.len() == 10 => {
            let digest = unfield(p[6])?;
            if digest.len() != 64 {
                return None;
            }
            TargetSource::Receipt {
                executable: unfield(p[5])?,
                digest,
            }
        }
        _ => return None,
    };
    let receipt = if p.get(p.len().saturating_sub(3)) == Some(&"d") {
        Some(ReceiptIdentity {
            executable: unfield(p[p.len() - 2])?,
            digest: unfield(p[p.len() - 1])?,
        })
    } else if p.last() == Some(&"n") {
        None
    } else {
        return None;
    };
    if let Some(r) = &receipt {
        if r.digest.len() != 64 {
            return None;
        }
    }
    Some(Target {
        action,
        id,
        commit,
        source,
        receipt,
    })
}
fn confirm_token(t: &Target) -> String {
    format!("confirm:{}", target_token(t))
}
fn update_token(old: &Target, candidate: &Target) -> String {
    format!(
        "update:{}:{}",
        hex(target_token(old).as_bytes()),
        hex(target_token(candidate).as_bytes())
    )
}
fn parse_update_token(value: &str) -> Option<(Target, Target)> {
    let rest = value.strip_prefix("update:")?;
    let (old, candidate) = rest.split_once(':')?;
    Some((
        parse_target(&String::from_utf8(unhex(old)?).ok()?)?,
        parse_target(&String::from_utf8(unhex(candidate)?).ok()?)?,
    ))
}
fn confirmation_token(c: &Confirmation) -> String {
    c.update_old.as_ref().map_or_else(
        || confirm_token(&c.target),
        |old| {
            format!(
                "confirm-update:{}:{}",
                target_token(old),
                target_token(&c.target)
            )
        },
    )
}
fn confirmation_cancel_token(c: &Confirmation) -> String {
    c.update_old.as_ref().map_or_else(
        || cancel_token(&c.target),
        |old| {
            format!(
                "cancel-update:{}:{}",
                target_token(old),
                target_token(&c.target)
            )
        },
    )
}
fn cancel_token(t: &Target) -> String {
    format!("cancel:{}", target_token(t))
}
fn target_valid(t: &Target) -> bool {
    t.id != MANAGER_ID
        && plugin_manager::validate_plugin_id(&t.id).is_ok()
        && plugin_manager::validate_commit(&t.commit).is_ok()
}
fn catalog_target(e: &CatalogEntry, action: Action) -> Option<Target> {
    let platform = std::env::consts::OS.to_owned();
    let executable = e.executables.get(&platform)?.to_string_lossy().into_owned();
    Some(Target {
        action,
        id: e.id.clone(),
        commit: e.commit.clone(),
        source: TargetSource::Catalog {
            repository: e.repository.clone(),
            executable,
            platform,
        },
        receipt: None,
    })
}
fn receipt_target(r: &plugin_manager::Receipt, action: Action) -> Option<Target> {
    Some(Target {
        action,
        id: r.id.clone(),
        commit: r.commit.clone(),
        source: TargetSource::Receipt {
            executable: r.executable.to_string_lossy().into_owned(),
            digest: r.digest.clone()?,
        },
        receipt: Some(ReceiptIdentity {
            executable: r.executable.to_string_lossy().into_owned(),
            digest: r.digest.clone()?,
        }),
    })
}
fn read_capped(path: &std::path::Path, cap: usize) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((cap + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds manager cap",
        ));
    }
    Ok(bytes)
}
fn real_directory(meta: &Metadata) -> bool {
    meta.is_dir() && !meta.file_type().is_symlink()
}
fn bounded_row(row: &str) -> (String, bool) {
    if row.len() <= MAX_ROW_BYTES {
        return (row.to_owned(), false);
    }
    let mut end = MAX_ROW_BYTES.saturating_sub(3);
    while end > 0 && !row.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…", &row[..end]), true)
}
fn build_widget(
    mut rows: Vec<String>,
    mut ids: Vec<Option<String>>,
) -> (Vec<String>, Vec<Option<String>>, Vec<u8>, bool) {
    let mut truncated = false;
    ids.truncate(rows.len());
    if rows.len() != ids.len() {
        ids.resize(rows.len(), None);
    }
    if rows.len() > MAX_WIDGET_ROWS {
        rows.truncate(MAX_WIDGET_ROWS);
        ids.truncate(MAX_WIDGET_ROWS);
        truncated = true;
    }
    for row in &mut rows {
        let (bounded, was_truncated) = bounded_row(row);
        *row = bounded;
        truncated |= was_truncated;
    }
    let mut payload = Vec::with_capacity(3 + rows.len() * 8);
    payload.push(1);
    payload.extend_from_slice(&(rows.len() as u16).to_le_bytes());
    for row in &rows {
        let bytes = row.as_bytes();
        payload.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        payload.extend_from_slice(bytes);
    }
    if payload.len() > MAX_WIDGET_PAYLOAD {
        // MAX_ROW_BYTES and MAX_WIDGET_ROWS currently make this unreachable,
        // but retain a checked fallback if either limit changes later.
        while payload.len() > MAX_WIDGET_PAYLOAD && rows.len() > 1 {
            rows.pop();
            ids.pop();
            truncated = true;
            payload.clear();
            payload.push(1);
            payload.extend_from_slice(&(rows.len() as u16).to_le_bytes());
            for row in &rows {
                let bytes = row.as_bytes();
                payload.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                payload.extend_from_slice(bytes);
            }
        }
    }
    (rows, ids, payload, truncated)
}
fn status_frame(text: &str) -> Frame {
    let mut s: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.len() > NOTICE_LIMIT {
        let mut end = NOTICE_LIMIT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    Frame {
        msg_type: STATUS,
        flags: 0,
        request_id: 0,
        resource_id: 0,
        resource_revision: 0,
        payload: s.into_bytes(),
    }
}
fn send<W: Write>(out: &mut W, frame: Frame) -> io::Result<()> {
    let mut b = Vec::new();
    encode(&frame, &mut b);
    out.write_all(&b)?;
    out.flush()
}
fn proto_io(e: plugin::ProtoError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_ROOTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn test_target(action: Action) -> Target {
        Target {
            action,
            id: "demo".into(),
            commit: "a".repeat(40),
            source: TargetSource::Receipt {
                executable: "payload".into(),
                digest: "b".repeat(64),
            },
            receipt: Some(ReceiptIdentity {
                executable: "payload".into(),
                digest: "b".repeat(64),
            }),
        }
    }
    fn test_target_with(id: &str, digest: String) -> Target {
        let mut t = test_target(Action::Remove);
        t.id = id.into();
        t.commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        t.source = TargetSource::Receipt {
            executable: "payload".into(),
            digest: digest.clone(),
        };
        t.receipt = Some(ReceiptIdentity {
            executable: "payload".into(),
            digest,
        });
        t
    }
    fn verified_demo(action: Action) -> (App, Target, std::path::PathBuf) {
        let root = std::path::PathBuf::from("/private/tmp").join(format!(
            "teddy-ui-state-{}-{}-{}",
            std::process::id(),
            action as u8,
            TEST_ROOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let paths = ManagerPaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let version = paths
            .data
            .join("demo")
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        fs::create_dir_all(&version).unwrap();
        let payload = version.join("payload");
        fs::write(&payload, b"payload").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&payload, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let digest = payload_digest(&payload).unwrap();
        plugin_manager::write_receipt_atomic(
            &version.join("receipt"),
            &Receipt {
                id: "demo".into(),
                commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                executable: "payload".into(),
                digest: Some(digest.clone()),
            },
        )
        .unwrap();
        let mut app = App::new();
        app.paths = Some(paths);
        app.core_records.push(plugin::LauncherRecord {
            id: "demo".into(),
            path: payload.display().to_string(),
            enabled: false,
            state: 4,
            max_restarts: 0,
            backoff_ms: 0,
        });
        let mut target = test_target_with("demo", digest);
        target.action = action;
        (app, target, root)
    }

    fn candidate_running_recovery() -> (App, Target, std::path::PathBuf) {
        let (mut app, candidate, root) = verified_demo(Action::Enable);
        let paths = app.paths.clone().unwrap();
        let payload = paths
            .data
            .join("demo")
            .join(&candidate.commit)
            .join("payload");
        let old_commit = "b".repeat(40);
        let old_version = paths.data.join("demo").join(&old_commit);
        fs::create_dir_all(&old_version).unwrap();
        let old_payload = old_version.join("payload");
        fs::write(&old_payload, b"old payload").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&old_payload, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_digest = payload_digest(&old_payload).unwrap();
        plugin_manager::write_receipt_atomic(
            &old_version.join("receipt"),
            &Receipt {
                id: candidate.id.clone(),
                commit: old_commit,
                executable: "payload".into(),
                digest: Some(old_digest.clone()),
            },
        )
        .unwrap();
        let old_receipt = Receipt {
            id: candidate.id.clone(),
            commit: "b".repeat(40),
            executable: "payload".into(),
            digest: Some(old_digest),
        };
        let candidate_receipt = Receipt {
            id: candidate.id.clone(),
            commit: candidate.commit.clone(),
            executable: "payload".into(),
            digest: candidate.receipt.as_ref().unwrap().digest.clone().into(),
        };
        let old_identity = UpdateIdentity {
            receipt: old_receipt,
            payload: old_payload.display().to_string().into(),
        };
        let candidate_identity = UpdateIdentity {
            receipt: candidate_receipt,
            payload: payload.display().to_string().into(),
        };
        let lock = ManagerLock::acquire_for(&paths).unwrap();
        let mut transaction =
            begin_update(&paths, &lock, old_identity, candidate_identity).unwrap();
        for stage in [
            UpdateStage::OldQuiesced,
            UpdateStage::CandidateEnableRequested,
            UpdateStage::CandidateRunning,
        ] {
            transaction = advance_update(&paths, &lock, &transaction, stage).unwrap();
        }
        drop(lock);
        app.active_update = None;
        (app, candidate, root)
    }

    fn candidate_record(target: &Target, enabled: bool, state: u8) -> plugin::LauncherRecord {
        plugin::LauncherRecord {
            id: target.id.clone(),
            path: target
                .receipt
                .as_ref()
                .map(|r| format!("/payload/{}", r.digest))
                .unwrap_or_default(),
            enabled,
            state,
            max_restarts: 0,
            backoff_ms: 0,
        }
    }

    #[test]
    fn failed_stage_persistence_stops_every_update_handoff() {
        let old = test_target(Action::Enable);
        let candidate = test_target_with("demo", "c".repeat(64));
        let receipt = |commit: &str, digest: &str| Receipt {
            id: "demo".into(),
            commit: commit.into(),
            executable: "payload".into(),
            digest: Some(digest.into()),
        };
        let transaction = UpdateTransaction {
            id: "demo".into(),
            old: UpdateIdentity {
                receipt: receipt(&old.commit, "b".repeat(64).as_str()),
                payload: "/old/payload".into(),
            },
            candidate: UpdateIdentity {
                receipt: receipt(&candidate.commit, &"c".repeat(64)),
                payload: "/candidate/payload".into(),
            },
            stage: UpdateStage::CandidateInstalled,
        };
        let mut app = App::new();
        app.active_update = Some(ActiveUpdate {
            transaction,
            old,
            candidate,
        });
        let mut out = Vec::new();
        assert!(!app
            .advance_active(UpdateStage::OldQuiesced, &mut out)
            .unwrap());
        assert!(app.controls.is_empty());
        assert!(app.active_update.is_some());
        assert!(!String::from_utf8_lossy(&out).contains("completed"));
        app.update_old_forgotten(&mut out).unwrap();
        assert!(app.controls.is_empty());
        app.begin_update_rollback(&mut out).unwrap();
        assert!(app.controls.is_empty());
    }

    #[test]
    fn candidate_running_recovery_completes_only_exact_running_candidate() {
        let (mut app, target, root) = candidate_running_recovery();
        let paths = app.paths.clone().unwrap();
        let payload = paths.data.join("demo").join(&target.commit).join("payload");
        let mut out = Vec::new();
        app.reconcile_pending_update(
            &[plugin::LauncherRecord {
                path: payload.display().to_string(),
                ..candidate_record(&target, true, 1)
            }],
            &mut out,
        )
        .unwrap();
        assert!(app.active_update.is_none());
        assert!(app.controls.is_empty());
        assert!(pending_update(&paths).unwrap().is_none());
        assert!(String::from_utf8_lossy(&out).contains("completed"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn candidate_running_recovery_rolls_back_absent_or_failed_candidate() {
        for state in [None, Some(4)] {
            let (mut app, target, root) = candidate_running_recovery();
            let paths = app.paths.clone().unwrap();
            let records = state
                .map(|state| vec![candidate_record(&target, true, state)])
                .unwrap_or_default();
            let mut out = Vec::new();
            app.reconcile_pending_update(&records, &mut out).unwrap();
            assert_eq!(app.controls.len(), 1);
            assert_eq!(app.pending_lists.len(), 0);
            assert_eq!(
                pending_update(&paths).unwrap().unwrap().stage,
                UpdateStage::RollbackRequested
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn candidate_running_recovery_polls_transient_candidate_without_rollback() {
        let (mut app, target, root) = candidate_running_recovery();
        let paths = app.paths.clone().unwrap();
        let mut out = Vec::new();
        app.reconcile_pending_update(&[candidate_record(&target, true, 0)], &mut out)
            .unwrap();
        assert!(app.controls.is_empty());
        assert_eq!(app.pending_lists.len(), 1);
        assert_eq!(
            pending_update(&paths).unwrap().unwrap().stage,
            UpdateStage::CandidateRunning
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn candidate_running_stage_write_failure_retains_journal_and_no_controls() {
        let (mut app, _target, root) = candidate_running_recovery();
        let paths = app.paths.clone().unwrap();
        let transaction = pending_update(&paths).unwrap().unwrap();
        let mut broken = paths.clone();
        broken.state = root.join("missing-state");
        app.paths = Some(broken);
        app.active_update = Some(ActiveUpdate {
            old: test_target(Action::Enable),
            candidate: test_target(Action::Enable),
            transaction,
        });
        let mut out = Vec::new();
        assert!(!app
            .advance_active(UpdateStage::Completed, &mut out)
            .unwrap());
        assert!(app.controls.is_empty());
        assert_eq!(
            pending_update(&paths).unwrap().unwrap().stage,
            UpdateStage::CandidateRunning
        );
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn command_and_filter_are_plain() {
        assert_eq!(
            parse_command("plugins-catalog Rust Tools"),
            ("plugins-catalog", "Rust Tools")
        );
        assert!(contains("RustFmt", "rust"));
    }
    #[test]
    fn stale_selection_is_harmless() {
        let mut r = Resource::new(CATALOG);
        r.replace(vec!["x".into()], vec![Some("x".into())]);
        assert!(r.valid_selection(0, 0).is_none());
        assert_eq!(r.valid_selection(1, 0), Some("x"));
    }

    #[test]
    fn action_tokens_round_trip_the_exact_version_and_identity() {
        let target = test_target(Action::Enable);
        assert_eq!(parse_target(&target_token(&target)), Some(target));
    }

    #[test]
    fn action_vocabulary_is_explicit_and_cancel_is_safe() {
        assert_eq!(action_label(Action::Install), "Install (keeps disabled)");
        assert_eq!(action_label(Action::InstallEnable), "Install and enable");
        assert_eq!(action_label(Action::Enable), "Enable");
        assert_eq!(action_label(Action::Disable), "Disable");
        assert_eq!(action_label(Action::Remove), "Remove");
        let mut app = App::new();
        app.confirmations = Some(Confirmation {
            source: CATALOG,
            source_revision: 1,
            target: test_target(Action::Remove),
            update_old: None,
        });
        let mut out = Vec::new();
        app.confirm(1, 0, None, &mut out).unwrap();
        assert!(app.controls.is_empty());
    }

    #[test]
    fn stale_confirmation_and_manager_self_are_rejected() {
        let mut app = App::new();
        app.resources.get_mut(&CATALOG).unwrap().revision = 4;
        let c = Confirmation {
            source: CATALOG,
            source_revision: 3,
            target: test_target(Action::Enable),
            update_old: None,
        };
        let mut self_target = c.target.clone();
        self_target.id = MANAGER_ID.into();
        assert!(!app.valid_action_target(&Confirmation {
            target: self_target,
            ..c.clone()
        }));
        app.confirmations = Some(c);
        let mut out = Vec::new();
        app.confirm(1, 1, Some("wrong"), &mut out).unwrap();
        assert!(app.controls.is_empty());
        assert!(!out.is_empty());
    }

    #[test]
    fn existing_record_enable_is_id_only() {
        let mut app = App::new();
        app.core_records.push(plugin::LauncherRecord {
            id: "demo".into(),
            path: "/payload".into(),
            enabled: false,
            state: 4,
            max_restarts: 0,
            backoff_ms: 0,
        });
        assert!(app
            .enable_descriptor(&test_target(Action::Enable))
            .is_none());
    }

    #[test]
    fn existing_record_must_match_the_exact_verified_payload_path() {
        let root = std::path::PathBuf::from("/private/tmp")
            .join(format!("teddy-ui-enable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = ManagerPaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let version = paths
            .data
            .join("demo")
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        fs::create_dir_all(&version).unwrap();
        let payload = version.join("payload");
        fs::write(&payload, b"payload").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&payload, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let digest = payload_digest(&payload).unwrap();
        plugin_manager::write_receipt_atomic(
            &version.join("receipt"),
            &Receipt {
                id: "demo".into(),
                commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                executable: "payload".into(),
                digest: Some(digest.clone()),
            },
        )
        .unwrap();
        let mut app = App::new();
        app.paths = Some(paths);
        app.core_records.push(plugin::LauncherRecord {
            id: "demo".into(),
            path: payload.display().to_string(),
            enabled: false,
            state: 4,
            max_restarts: 0,
            backoff_ms: 0,
        });
        let target = test_target_with("demo", digest);
        assert_eq!(
            app.enable_request(&target),
            Some(LauncherRequest::Enable {
                id: "demo".into(),
                descriptor: None
            })
        );
        app.core_records[0].path = "/external/plugin".into();
        assert!(app.enable_request(&target).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mismatched_launcher_response_cannot_start_poll() {
        let mut app = App::new();
        let target = test_target(Action::Enable);
        app.controls.insert(
            7,
            Control {
                id: "demo".into(),
                action: Action::Enable,
                target,
            },
        );
        let frame = plugin::encode_launcher_response(7, &LauncherResponse::Enabled("other".into()))
            .unwrap();
        let mut out = Vec::new();
        app.launcher_response(frame, &mut out).unwrap();
        assert!(app.pending_lists.is_empty());
        assert!(String::from_utf8_lossy(&out).contains("different"));
    }

    #[test]
    fn advisory_event_cannot_consume_a_pending_response() {
        let mut app = App::new();
        app.controls.insert(
            7,
            Control {
                id: "demo".into(),
                action: Action::Enable,
                target: test_target(Action::Enable),
            },
        );
        let event =
            plugin::encode_launcher_event(&plugin::LauncherEvent::Enabled("demo".into())).unwrap();
        let mut out = Vec::new();
        app.launcher_event(event, &mut out).unwrap();
        assert!(app.controls.contains_key(&7));
        assert!(app.pending_lists.is_empty());
        let response =
            plugin::encode_launcher_response(7, &LauncherResponse::Enabled("demo".into())).unwrap();
        app.launcher_response(response, &mut out).unwrap();
        assert!(!app.controls.contains_key(&7));
        assert!(app.pending_lists.contains_key(&7));
    }

    #[test]
    fn remove_waits_through_all_transient_host_states() {
        for state in [0, 2, 3] {
            let (mut app, target, root) = verified_demo(Action::Remove);
            let mut out = Vec::new();
            let path = app.record_for("demo").unwrap().path.clone();
            app.lifecycle_observed(
                target,
                0,
                &[plugin::LauncherRecord {
                    id: "demo".into(),
                    path,
                    enabled: false,
                    state,
                    max_restarts: 0,
                    backoff_ms: 0,
                }],
                &mut out,
            )
            .unwrap();
            assert!(
                app.controls.is_empty(),
                "state {state} must not be forgotten"
            );
            assert_eq!(
                app.pending_lists.len(),
                1,
                "state {state} must continue bounded polling"
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn remove_exact_delegates_to_manager_and_handles_not_found() {
        let root = std::path::PathBuf::from("/private/tmp")
            .join(format!("teddy-ui-remove-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = ManagerPaths {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        fs::create_dir_all(
            paths
                .data
                .join("demo")
                .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        )
        .unwrap();
        let version = paths
            .data
            .join("demo")
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let payload = version.join("payload");
        fs::write(&payload, b"safe payload").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = fs::metadata(&payload).unwrap().permissions();
            p.set_mode(0o755);
            fs::set_permissions(&payload, p).unwrap();
        }
        let digest = payload_digest(&payload).unwrap();
        plugin_manager::write_receipt_atomic(
            &version.join("receipt"),
            &Receipt {
                id: "demo".into(),
                commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                executable: "payload".into(),
                digest: Some(digest.clone()),
            },
        )
        .unwrap();
        let mut app = App::new();
        app.paths = Some(paths.clone());
        let mut out = Vec::new();
        app.remove_exact(&test_target_with("demo", digest), &mut out);
        assert!(!version.exists());
        let absent = Target {
            action: Action::Remove,
            id: "demo".into(),
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            source: TargetSource::Receipt {
                executable: "payload".into(),
                digest: "b".repeat(64),
            },
            receipt: Some(ReceiptIdentity {
                executable: "payload".into(),
                digest: "b".repeat(64),
            }),
        };
        app.remove_exact(&absent, &mut out);
        assert!(
            String::from_utf8_lossy(&out).contains("already absent")
                || String::from_utf8_lossy(&out).contains("could not")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_poll_requires_reaping_before_forget() {
        let mut app = App::new();
        app.controls.insert(
            7,
            Control {
                id: "demo".into(),
                action: Action::Remove,
                target: test_target(Action::Remove),
            },
        );
        let disabled =
            plugin::encode_launcher_response(7, &LauncherResponse::Disabled("demo".into()))
                .unwrap();
        let mut out = Vec::new();
        app.launcher_response(disabled, &mut out).unwrap();
        let list = list_frame(7, 0, None);
        app.launcher_response(list, &mut out).unwrap();
        assert!(app.controls.is_empty());
        assert!(app.pending_lists.is_empty());
    }

    #[test]
    fn lifecycle_poll_reports_running_and_failed_without_payloads() {
        let (mut app, target, root) = verified_demo(Action::Enable);
        app.pending_lists.insert(
            2,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Lifecycle {
                    target: target.clone(),
                    poll: 0,
                },
            },
        );
        let running = plugin::encode_launcher_response(
            2,
            &LauncherResponse::List {
                page: 0,
                next_page: None,
                records: vec![plugin::LauncherRecord {
                    id: "demo".into(),
                    path: app.record_for("demo").unwrap().path.clone(),
                    enabled: true,
                    state: 1,
                    max_restarts: 0,
                    backoff_ms: 0,
                }],
            },
        )
        .unwrap();
        let mut out = Vec::new();
        assert!(
            app.record_path_matches_target(&target, app.record_for("demo").unwrap().path.as_str())
        );
        app.launcher_response(running, &mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("confirmed"));
        app.pending_lists.insert(
            3,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Lifecycle { target, poll: 0 },
            },
        );
        let failed = plugin::encode_launcher_response(
            3,
            &LauncherResponse::List {
                page: 0,
                next_page: None,
                records: vec![plugin::LauncherRecord {
                    id: "demo".into(),
                    path: app.record_for("demo").unwrap().path.clone(),
                    enabled: true,
                    state: 4,
                    max_restarts: 0,
                    backoff_ms: 0,
                }],
            },
        )
        .unwrap();
        app.launcher_response(failed, &mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("failed"));
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn list_pages_aggregate_by_request() {
        let mut a = App::new();
        a.pending_lists.insert(
            7,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 1,
                collected: vec![plugin::LauncherRecord {
                    id: "a".into(),
                    path: "/a".into(),
                    enabled: false,
                    state: 4,
                    max_restarts: 0,
                    backoff_ms: 0,
                }],
                purpose: ListPurpose::Core,
            },
        );
        assert_eq!(a.pending_lists.get(&7).unwrap().collected.len(), 1);
    }

    #[test]
    fn oversized_widget_rows_are_bounded_without_panicking() {
        let rows = (0..(MAX_WIDGET_ROWS + 20))
            .map(|_| "é".repeat(MAX_ROW_BYTES))
            .collect();
        let ids = vec![None; MAX_WIDGET_ROWS + 20];
        let (rows, _, payload, truncated) = build_widget(rows, ids);
        assert!(truncated);
        assert!(rows.len() <= MAX_WIDGET_ROWS);
        assert!(rows.iter().all(|r| r.len() <= MAX_ROW_BYTES));
        assert!(payload.len() <= MAX_WIDGET_PAYLOAD);
        assert_eq!(
            plugin::parse_widget(&payload).unwrap().items.len(),
            rows.len()
        );
    }

    #[test]
    fn notices_are_bounded_on_utf8_boundaries() {
        let frame = status_frame(&"é".repeat(NOTICE_LIMIT));
        assert!(frame.payload.len() <= NOTICE_LIMIT);
        assert!(std::str::from_utf8(&frame.payload).is_ok());
    }

    #[test]
    fn oversized_catalog_read_is_rejected_before_parsing() {
        let path = std::env::temp_dir().join(format!("teddy-manager-cap-{}", std::process::id()));
        fs::write(&path, vec![b'x'; MAX_CATALOG_BYTES + 1]).unwrap();
        assert!(read_capped(&path, MAX_CATALOG_BYTES).is_err());
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_invalid_version_entries_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("teddy-manager-links-{}", std::process::id()));
        let target = root.join("target");
        let link = root.join("link");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(!real_directory(&meta));
        assert!(plugin_manager::validate_commit("not-a-commit").is_err());
        let _ = fs::remove_file(link);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installed_scan_has_one_global_budget_across_directories_and_versions() {
        let root = std::env::temp_dir().join(format!("teddy-manager-scan-{}", std::process::id()));
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let commit = "0123456789abcdef0123456789abcdef01234567";
        for i in 0..80 {
            let plugin_dir = data.join(format!("plugin{i}"));
            let version_dir = plugin_dir.join(commit);
            fs::create_dir_all(&version_dir).unwrap();
            fs::write(
                version_dir.join("receipt"),
                format!(
                    "format=teddy-receipt.v1\nid=plugin{i}\ncommit={commit}\nexecutable=bin/plugin\n"
                ),
            )
            .unwrap();
            for j in 0..30 {
                fs::create_dir_all(plugin_dir.join(format!("invalid-version-{j}"))).unwrap();
                fs::write(plugin_dir.join(format!("not-a-directory-{j}")), b"ignored").unwrap();
            }
        }

        let mut app = App::new();
        app.paths = Some(ManagerPaths {
            config: root.join("config"),
            data,
            state: root.join("state"),
        });
        let mut out = Vec::new();
        app.show_installed("", &mut out).unwrap();
        let resource = app.resources.get(&INSTALLED).unwrap();
        assert!(resource.rows.len() <= MAX_WIDGET_ROWS);
        assert!(String::from_utf8_lossy(&out).contains("truncated"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installed_budget_counts_raw_and_constructed_work() {
        let mut budget = InstalledBudget::new();
        for _ in 0..MAX_INSTALLED_SCAN_WORK {
            assert!(budget.take(1));
        }
        assert!(!budget.take(1));
        let mut budget = InstalledBudget::new();
        assert!(budget.take(1)); // raw directory result
        assert!(budget.take(1)); // raw version result
        assert!(budget.take(1 + INSTALLED_ROWS_PER_RECEIPT)); // receipt plus rows
        assert_eq!(budget.used, 9);
    }

    fn list_frame(request_id: u32, page: u16, next_page: Option<u16>) -> Frame {
        plugin::encode_launcher_response(
            request_id,
            &LauncherResponse::List {
                page,
                next_page,
                records: Vec::new(),
            },
        )
        .unwrap()
    }

    #[test]
    fn list_rejects_unknown_duplicate_and_cyclic_pages() {
        let mut app = App::new();
        let mut out = Vec::new();
        app.launcher_response(list_frame(99, 0, None), &mut out)
            .unwrap();
        assert!(app.pending_lists.is_empty());

        app.pending_lists.insert(
            7,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Core,
            },
        );
        app.launcher_response(list_frame(7, 0, None), &mut out)
            .unwrap();
        assert!(app.pending_lists.is_empty());
        app.launcher_response(list_frame(7, 0, None), &mut out)
            .unwrap();

        app.pending_lists.insert(
            8,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Core,
            },
        );
        app.launcher_response(list_frame(8, 0, Some(0)), &mut out)
            .unwrap();
        assert!(app.pending_lists.is_empty());
    }

    #[test]
    fn list_rejects_out_of_order_and_error_responses() {
        let mut app = App::new();
        let mut out = Vec::new();
        app.pending_lists.insert(
            9,
            PendingList {
                expected_page: 1,
                pages: 1,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Core,
            },
        );
        app.launcher_response(list_frame(9, 0, None), &mut out)
            .unwrap();
        assert!(app.pending_lists.is_empty());

        app.pending_lists.insert(
            10,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Core,
            },
        );
        let frame =
            plugin::encode_launcher_response(10, &LauncherResponse::Error("no state".into()))
                .unwrap();
        app.launcher_response(frame, &mut out).unwrap();
        assert!(app.pending_lists.is_empty());
        assert!(!out.is_empty());
    }

    #[test]
    fn list_accepts_only_the_expected_forward_page() {
        let mut app = App::new();
        let mut out = Vec::new();
        app.pending_lists.insert(
            11,
            PendingList {
                expected_page: 0,
                pages: 0,
                records: 0,
                collected: Vec::new(),
                purpose: ListPurpose::Core,
            },
        );
        app.launcher_response(list_frame(11, 0, Some(1)), &mut out)
            .unwrap();
        assert_eq!(app.pending_lists.get(&11).unwrap().expected_page, 1);
        app.launcher_response(list_frame(11, 1, None), &mut out)
            .unwrap();
        assert!(app.pending_lists.is_empty());
    }
}
