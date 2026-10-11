//! Keeping damaged and previous files around.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Renames a damaged `config.toml` to `config.broken-<unix-ts>.toml` beside it and returns the
/// new path. Never overwrites: a name that exists gets a `-<n>` suffix.
pub fn move_aside(path: &Path) -> io::Result<PathBuf> {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("config");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("toml");
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut n = 0u32;
    loop {
        let name = if n == 0 { format!("{stem}.broken-{ts}.{ext}") } else { format!("{stem}.broken-{ts}-{n}.{ext}") };
        let target = dir.join(name);
        if target.exists() {
            n = n.saturating_add(1);
            if n > 1000 {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "too many broken copies"));
            }
            continue;
        }
        std::fs::rename(path, &target)?;
        return Ok(target);
    }
}

/// The backup path for a config file: `config.toml` → `config.toml.bak`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".bak");
    path.with_file_name(name)
}

/// Copies the current file to its backup, atomically. Nothing happens when there is no file.
pub fn refresh_backup(path: &Path) -> io::Result<()> {
    match std::fs::read(path) {
        Ok(bytes) => craft_atomic::atomic_write(&backup_path(path), &bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Checks that `dir` accepts a new file by creating and removing a probe.
pub fn check_writable(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".craft-settings-probe-{}", std::process::id()));
    std::fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
    std::fs::remove_file(&probe)
}
