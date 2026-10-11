//! The in-memory store: load once, read every frame, save while the application runs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use toml_edit::DocumentMut;

use crate::Settings;
use crate::document::{self, merge_into, set_schema_version};
use crate::error::{LoadReport, ReadOnlyReason, SaveStatus, SettingsError, Source, TickResult};
use crate::migrate::{self, LegacyFile, Upgrade};
use crate::paths::{self, Location, Probe};
use crate::recovery;

/// The settings file name.
pub const CONFIG_FILE: &str = "config.toml";

/// How long after the last change a save is attempted.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);

/// First retry delay after a failed save; each failure doubles it, up to [`MAX_RETRY_DELAY`].
pub const FIRST_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Longest wait between retries.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// How to open a store.
pub struct OpenOptions<T> {
    /// Where the settings live.
    pub location: Location,
    /// Older files to import when there is no `config.toml` yet.
    pub legacy: Vec<LegacyFile<T>>,
    /// Quiet period after a change before saving.
    pub debounce: Duration,
    /// What the folder rules look at.
    pub probe: Probe,
}

impl<T> Default for OpenOptions<T> {
    fn default() -> Self {
        Self { location: Location::Auto, legacy: Vec::new(), debounce: DEFAULT_DEBOUNCE, probe: Probe::default() }
    }
}

impl<T> OpenOptions<T> {
    /// Options for `location`.
    pub fn new(location: Location) -> Self {
        Self { location, ..Self::default() }
    }

    /// Older files to import on first run.
    pub fn legacy(mut self, files: Vec<LegacyFile<T>>) -> Self {
        self.legacy = files;
        self
    }

    /// Quiet period after a change before saving.
    pub fn debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    /// What the folder rules look at.
    pub fn probe(mut self, probe: Probe) -> Self {
        self.probe = probe;
        self
    }
}

/// An application's settings, with the file they came from.
pub struct SettingsStore<T: Settings> {
    dir: PathBuf,
    path: PathBuf,
    doc: DocumentMut,
    value: T,
    read_only: Option<String>,
    dirty: bool,
    written: bool,
    last_change: Option<Instant>,
    debounce: Duration,
    failures: u32,
    retry_at: Option<Instant>,
    seen_mtime: Option<SystemTime>,
}

impl<T: Settings> std::fmt::Debug for SettingsStore<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsStore")
            .field("path", &self.path)
            .field("read_only", &self.read_only)
            .field("dirty", &self.dirty)
            .finish_non_exhaustive()
    }
}

impl<T: Settings> SettingsStore<T> {
    /// Opens the store for `location`. Never fails: problems come back in the report.
    pub fn open(location: Location) -> (Self, LoadReport) {
        Self::open_with(OpenOptions::new(location))
    }

    /// Opens a store in `dir`, for settings that travel with a folder such as a library.
    pub fn open_at(dir: impl Into<PathBuf>) -> (Self, LoadReport) {
        Self::open(Location::Dir(dir.into()))
    }

    /// Opens the store with full options.
    pub fn open_with(options: OpenOptions<T>) -> (Self, LoadReport) {
        let resolved = paths::resolve(T::APP_ID, &options.location, &options.probe);
        let path = resolved.dir.join(CONFIG_FILE);
        let mut report = LoadReport {
            dir: resolved.dir.clone(),
            rule: resolved.rule.to_string(),
            source: Source::Defaults,
            broken_moved_to: None,
            section_errors: Vec::new(),
            migrated_from: None,
            read_only: None,
        };
        let mut store = Self {
            dir: resolved.dir,
            path,
            doc: document::new_document(T::SCHEMA_VERSION),
            value: T::default(),
            read_only: None,
            dirty: false,
            written: false,
            last_change: None,
            debounce: options.debounce,
            failures: 0,
            retry_at: None,
            seen_mtime: None,
        };

        if let Err(e) = std::fs::create_dir_all(&store.dir) {
            store.load_best_effort(&mut report);
            store.set_read_only(&mut report, ReadOnlyReason::DirUnavailable(e));
            return (store, report);
        }
        if let Err(e) = recovery::check_writable(&store.dir) {
            store.load_best_effort(&mut report);
            store.set_read_only(&mut report, ReadOnlyReason::DirNotWritable(e));
            return (store, report);
        }

        match std::fs::read_to_string(&store.path) {
            Ok(text) => store.load_text(&text, &mut report),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some((value, from)) = migrate::import_legacy(&options.legacy) {
                    store.value = value;
                    store.dirty = true;
                    report.source = Source::Legacy(from);
                }
            }
            Err(e) => store.set_read_only(&mut report, ReadOnlyReason::Unreadable(e)),
        }
        (store, report)
    }

    /// The configuration folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The `config.toml` path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current settings. In memory; cheap to call every frame.
    pub fn get(&self) -> &T {
        &self.value
    }

    /// True when the store will not write the file; the load report says why.
    pub fn is_read_only(&self) -> bool {
        self.read_only.is_some()
    }

    /// True when changes are waiting to be saved.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Changes the settings. Returns true, and schedules a save, only if the value changed.
    pub fn update(&mut self, f: impl FnOnce(&mut T)) -> bool {
        let mut next = self.value.clone();
        f(&mut next);
        self.set(next)
    }

    /// Replaces the settings. Returns true, and schedules a save, only if the value changed.
    pub fn set(&mut self, value: T) -> bool {
        if value == self.value {
            return false;
        }
        self.value = value;
        self.mark_changed(Instant::now());
        true
    }

    /// Like [`SettingsStore::update`] with an explicit clock, for tests.
    pub fn update_at(&mut self, now: Instant, f: impl FnOnce(&mut T)) -> bool {
        let mut next = self.value.clone();
        f(&mut next);
        if next == self.value {
            return false;
        }
        self.value = next;
        self.mark_changed(now);
        true
    }

    /// Saves pending changes once the debounce or retry delay has passed. Call every frame
    /// and again at `next_deadline`, so an idle application still saves.
    pub fn tick(&mut self, now: Instant) -> TickResult {
        if !self.dirty || self.read_only.is_some() {
            return TickResult { status: SaveStatus::Idle, next_deadline: None };
        }
        let due = match (self.retry_at, self.last_change) {
            (Some(retry), _) => retry,
            (None, Some(changed)) => changed + self.debounce,
            (None, None) => now,
        };
        if now < due {
            return TickResult { status: SaveStatus::Pending, next_deadline: Some(due) };
        }
        match self.save() {
            Ok(()) => TickResult { status: SaveStatus::Saved, next_deadline: None },
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                let delay = retry_delay(self.failures);
                self.retry_at = Some(now + delay);
                TickResult { status: SaveStatus::Failed(e), next_deadline: Some(now + delay) }
            }
        }
    }

    /// Saves now. Writes the file even when nothing changed if it does not exist yet, so users
    /// can find it. Call on exit.
    pub fn flush(&mut self) -> Result<(), SettingsError> {
        if let Some(why) = &self.read_only {
            return Err(SettingsError::ReadOnly(why.clone()));
        }
        if !self.dirty && (self.written || self.path.exists()) {
            return Ok(());
        }
        self.save()
    }

    fn mark_changed(&mut self, now: Instant) {
        self.dirty = true;
        self.last_change = Some(now);
    }

    fn set_read_only(&mut self, report: &mut LoadReport, reason: ReadOnlyReason) {
        self.read_only = Some(reason.to_string());
        report.read_only = Some(reason);
    }

    /// Reads the file if it happens to be readable, so a read-only store still shows the
    /// user's values instead of defaults.
    fn load_best_effort(&mut self, report: &mut LoadReport) {
        if let Ok(text) = std::fs::read_to_string(&self.path)
            && let Ok(mut doc) = document::parse(&text)
        {
            let upgraded = migrate::upgrade::<T>(&mut doc);
            let (value, errors) = document::deserialize_sections::<T>(&doc);
            self.value = value;
            self.doc = doc;
            report.source = Source::File;
            report.section_errors = errors;
            if let Ok(Upgrade::Migrated { from }) = upgraded {
                report.migrated_from = Some(from);
            }
        }
    }

    fn load_text(&mut self, text: &str, report: &mut LoadReport) {
        let mut doc = match document::parse(text) {
            Ok(doc) => doc,
            Err(_) => {
                match recovery::move_aside(&self.path) {
                    Ok(moved) => report.broken_moved_to = Some(moved),
                    Err(e) => self.set_read_only(report, ReadOnlyReason::CannotMoveAside(e)),
                }
                return;
            }
        };
        match migrate::upgrade::<T>(&mut doc) {
            Ok(Upgrade::Current) => {}
            Ok(Upgrade::Migrated { from }) => {
                report.migrated_from = Some(from);
                self.dirty = true;
            }
            Ok(Upgrade::Newer { file }) => self.set_read_only(report, ReadOnlyReason::NewerSchema { file, app: T::SCHEMA_VERSION }),
            Err(e) => {
                self.set_read_only(report, ReadOnlyReason::Migration(e));
                return;
            }
        }
        let (value, errors) = document::deserialize_sections::<T>(&doc);
        self.value = value;
        self.doc = doc;
        self.seen_mtime = mtime(&self.path);
        report.source = Source::File;
        report.section_errors = errors;
    }

    fn save(&mut self) -> Result<(), SettingsError> {
        if let Some(why) = &self.read_only {
            return Err(SettingsError::ReadOnly(why.clone()));
        }
        self.reload_if_changed_outside();
        recovery::refresh_backup(&self.path)?;
        merge_into(&mut self.doc, &self.value).map_err(|e| SettingsError::Serialize(e.to_string()))?;
        set_schema_version(&mut self.doc, T::SCHEMA_VERSION);
        let text = document::render(&self.doc);
        craft_atomic::atomic_write_with(&self.path, |file| {
            restrict_permissions(file)?;
            file.write_all(text.as_bytes())
        })?;
        self.seen_mtime = mtime(&self.path);
        self.dirty = false;
        self.written = true;
        self.failures = 0;
        self.retry_at = None;
        Ok(())
    }

    /// Another instance or a hand edit may have changed the file since it was read. Re-read it
    /// so their comments and unknown keys survive; known keys are then overwritten by this
    /// store's values.
    fn reload_if_changed_outside(&mut self) {
        let current = mtime(&self.path);
        if current.is_none() || current == self.seen_mtime {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&self.path)
            && let Ok(mut doc) = document::parse(&text)
            && !matches!(migrate::upgrade::<T>(&mut doc), Ok(Upgrade::Newer { .. }) | Err(_))
        {
            self.doc = doc;
        }
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Exponential backoff: 1 s, 2 s, 4 s, … capped at [`MAX_RETRY_DELAY`].
pub fn retry_delay(failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(16);
    (FIRST_RETRY_DELAY * 2u32.saturating_pow(exp)).min(MAX_RETRY_DELAY)
}

/// New files are owner-only; on a replace `craft-atomic` keeps the existing file's mode.
#[cfg(unix)]
fn restrict_permissions(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_permissions(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}
