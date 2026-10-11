mod common;

use common::{Theme, config_path, open_in, read_config, write_config};
use craft_settings::{ReadOnlyReason, SettingsError, Source};

#[test]
fn syntax_error_moves_the_file_aside_and_uses_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "schema_version = 2\n[general\nlanguage = \"de\"\n");
    let (mut store, report) = open_in(tmp.path());
    assert_eq!(report.source, Source::Defaults);
    assert!(!store.is_read_only());
    let moved = report.broken_moved_to.clone().expect("moved aside");
    assert!(moved.file_name().unwrap().to_str().unwrap().starts_with("config.broken-"), "{moved:?}");
    assert!(moved.exists());
    assert!(!config_path(tmp.path()).exists());
    assert_eq!(store.get().general.language, "en");

    store.flush().unwrap();
    assert!(moved.exists(), "the damaged copy is never deleted");
    assert!(read_config(tmp.path()).starts_with("schema_version = 2\n"));
}

#[test]
fn two_broken_files_get_different_names() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "not = [toml");
    let (_, first) = open_in(tmp.path());
    write_config(tmp.path(), "still = [broken");
    let (_, second) = open_in(tmp.path());
    assert_ne!(first.broken_moved_to, second.broken_moved_to);
    assert!(second.broken_moved_to.unwrap().exists());
}

#[test]
fn bad_value_only_resets_its_section() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "schema_version = 2\n[general]\nlanguage = \"de\"\n\n[appearance]\ntheme = \"sepia\"\nui_scale = 2.0\n");
    let (store, report) = open_in(tmp.path());
    assert_eq!(report.source, Source::File);
    assert!(report.broken_moved_to.is_none());
    assert!(!store.is_read_only());
    assert_eq!(report.section_errors.len(), 1, "{report:?}");
    assert_eq!(report.section_errors[0].section, "appearance");
    assert_eq!(store.get().general.language, "de", "the good section survives");
    assert_eq!(store.get().appearance.theme, Theme::Light, "the bad section falls back to defaults");
    assert!(config_path(tmp.path()).exists(), "the file is not moved aside");
}

#[test]
fn newer_schema_opens_read_only_and_never_overwrites() {
    let tmp = tempfile::tempdir().unwrap();
    let original = "schema_version = 9\n[general]\nlanguage = \"de\"\n";
    write_config(tmp.path(), original);
    let (mut store, report) = open_in(tmp.path());
    assert!(matches!(report.read_only, Some(ReadOnlyReason::NewerSchema { file: 9, app: 2 })), "{report:?}");
    assert_eq!(store.get().general.language, "de", "values are still shown");
    store.update(|p| p.general.language = "fr".into());
    assert!(matches!(store.flush(), Err(SettingsError::ReadOnly(_))));
    assert_eq!(read_config(tmp.path()), original);
}

#[test]
fn failed_migration_opens_read_only_and_leaves_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let original = "schema_version = 0\n[general]\nlanguage = \"de\"\n";
    write_config(tmp.path(), original);
    let (mut store, report) = open_in(tmp.path());
    assert!(matches!(report.read_only, Some(ReadOnlyReason::Migration(_))), "{report:?}");
    assert!(matches!(store.flush(), Err(SettingsError::ReadOnly(_))));
    assert_eq!(read_config(tmp.path()), original);
}

#[cfg(unix)]
#[test]
fn damaged_file_that_cannot_be_moved_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "not = [toml");
    // The folder passes the write probe only while it is writable, so open first, then lock it.
    let dir = tmp.path();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let (mut store, report) = open_in(dir);
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(store.is_read_only(), "{report:?}");
    assert!(matches!(store.flush(), Err(SettingsError::ReadOnly(_))));
    assert_eq!(read_config(dir), "not = [toml");
}
