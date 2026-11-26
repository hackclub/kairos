
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

pub const UUID: &str = "kairos-focus@obayy.dev";

const MODERN_EXTENSION: &str = include_str!("../../../packaging/gnome-extension/modern/extension.js");
const MODERN_METADATA: &str = include_str!("../../../packaging/gnome-extension/modern/metadata.json");
const LEGACY_EXTENSION: &str = include_str!("../../../packaging/gnome-extension/legacy/extension.js");
const LEGACY_METADATA: &str = include_str!("../../../packaging/gnome-extension/legacy/metadata.json");

/// GNOME 45 moved extensions to ES modules; 40 to 44 use the legacy `init()` entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Legacy,
    Modern,
}

impl Variant {
    pub fn for_version(output: &str) -> Option<Variant> {
        let major: u32 = output.split_whitespace().last()?.split('.').next()?.parse().ok()?;
        match major {
            45.. => Some(Variant::Modern),
            40..=44 => Some(Variant::Legacy),
            _ => None,
        }
    }

    fn files(self) -> [(&'static str, &'static str); 2] {
        match self {
            Variant::Modern => [("extension.js", MODERN_EXTENSION), ("metadata.json", MODERN_METADATA)],
            Variant::Legacy => [("extension.js", LEGACY_EXTENSION), ("metadata.json", LEGACY_METADATA)],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Active,
    /// Installed and enabled. A GNOME Shell session picks up new extensions at the next login
    /// (Wayland cannot restart the shell in place).
    ActiveAfterLogin,
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn listed(current: &str) -> Vec<&str> {
    current.split('\'').skip(1).step_by(2).collect()
}

fn gvariant_list(items: &[&str]) -> String {
    format!("[{}]", items.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", "))
}

/// `['a@x', 'b@y']` or `@as []` as printed by `gsettings get`.
fn with_extension(current: &str, uuid: &str) -> Option<String> {
    let mut items = listed(current);
    if items.contains(&uuid) {
        return None;
    }
    items.push(uuid);
    Some(gvariant_list(&items))
}

fn without_extension(current: &str, uuid: &str) -> Option<String> {
    let items = listed(current);
    let kept: Vec<&str> = items.iter().copied().filter(|i| *i != uuid).collect();
    (kept.len() != items.len()).then(|| gvariant_list(&kept))
}

fn write_files(dir: &Path, variant: Variant) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for (name, content) in variant.files() {
        fs::write(dir.join(name), content).with_context(|| format!("cannot write {name}"))?;
    }
    Ok(())
}

fn extension_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| anyhow!("no user data directory"))?;
    Ok(base.join("gnome-shell").join("extensions").join(UUID))
}

/// The shell only discovers new extensions at startup, so the extension is listed in `enabled-extensions`
/// directly (what `gnome-extensions enable` refuses to do for an extension the shell has not seen).
pub fn install_gnome_extension() -> Result<InstallOutcome> {
    let version = run("gnome-shell", &["--version"]).ok_or_else(|| anyhow!("gnome-shell is not installed"))?;
    let variant = Variant::for_version(&version).ok_or_else(|| anyhow!("unsupported GNOME Shell version"))?;
    write_files(&extension_dir()?, variant)?;

    let enabled = run("gsettings", &["get", "org.gnome.shell", "enabled-extensions"]);
    match enabled.as_deref().map(|c| with_extension(c, UUID)) {
        Some(Some(list)) => {
            if run("gsettings", &["set", "org.gnome.shell", "enabled-extensions", &list]).is_none() {
                bail!("cannot enable the extension in GNOME settings");
            }
        }
        Some(None) => {}
        None => {
            run("gnome-extensions", &["enable", UUID]).ok_or_else(|| anyhow!("cannot enable the extension"))?;
        }
    }

    let loaded = run("gnome-extensions", &["info", UUID]).is_some_and(|info| info.contains("State: ACTIVE"));
    Ok(if loaded { InstallOutcome::Active } else { InstallOutcome::ActiveAfterLogin })
}

/// Install writes the files once, so a newer kairosd brings an installed extension up to date. GNOME loads it at the
/// next login.
pub fn refresh_gnome_extension() -> Result<bool> {
    let dir = extension_dir()?;
    if !dir.is_dir() {
        return Ok(false);
    }
    let version = run("gnome-shell", &["--version"]).ok_or_else(|| anyhow!("gnome-shell is not installed"))?;
    let variant = Variant::for_version(&version).ok_or_else(|| anyhow!("unsupported GNOME Shell version"))?;
    refresh_files(&dir, variant)
}

fn refresh_files(dir: &Path, variant: Variant) -> Result<bool> {
    let stale = variant.files().iter().any(|(name, content)| fs::read_to_string(dir.join(name)).ok().as_deref() != Some(*content));
    if stale {
        write_files(dir, variant)?;
    }
    Ok(stale)
}

pub fn is_gnome_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|p| p.eq_ignore_ascii_case("gnome")))
}

pub fn uninstall_gnome_extension() -> Result<()> {
    if let Some(list) = run("gsettings", &["get", "org.gnome.shell", "enabled-extensions"])
        .as_deref()
        .and_then(|c| without_extension(c, UUID))
    {
        run("gsettings", &["set", "org.gnome.shell", "enabled-extensions", &list]);
    }
    let dir = extension_dir()?;
    match fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("cannot remove {}", dir.display())),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_45_and_newer_use_the_module_variant() {
        for v in ["GNOME Shell 45.0", "GNOME Shell 46.2", "GNOME Shell 49.beta", "GNOME Shell 51.1\n", "GNOME Shell 100.0"] {
            assert_eq!(Variant::for_version(v), Some(Variant::Modern), "{v}");
        }
    }

    #[test]
    fn gnome_40_to_44_use_the_legacy_variant() {
        for v in ["GNOME Shell 40.0", "GNOME Shell 42.9", "GNOME Shell 44.1"] {
            assert_eq!(Variant::for_version(v), Some(Variant::Legacy), "{v}");
        }
    }

    #[test]
    fn older_or_unreadable_versions_are_unsupported() {
        for v in ["GNOME Shell 3.38.6", "GNOME Shell 39.0", "", "GNOME Shell", "gnome-shell: command not found"] {
            assert_eq!(Variant::for_version(v), None, "{v}");
        }
    }

    #[test]
    fn each_variant_ships_its_own_entry_point_and_matching_metadata() {
        let [(_, modern_js), (_, modern_meta)] = Variant::Modern.files();
        let [(_, legacy_js), (_, legacy_meta)] = Variant::Legacy.files();
        assert!(modern_js.contains("export default class"));
        assert!(legacy_js.contains("function init()"));
        assert!(modern_meta.contains(UUID) && legacy_meta.contains(UUID));
        assert!(modern_meta.contains("\"45\"") && !modern_meta.contains("\"44\""));
        assert!(legacy_meta.contains("\"40\"") && !legacy_meta.contains("\"45\""));
    }

    #[test]
    fn the_extension_exports_what_the_daemon_calls() {
        for js in [MODERN_EXTENSION, LEGACY_EXTENSION] {
            assert!(js.contains("dev.obayy.Kairos.Focus1"));
            assert!(js.contains("'/dev/obayy/Kairos'"));
            assert!(js.contains("name=\"Get\"") && js.contains("name=\"GetWindow\"") && js.contains("name=\"Changed\""));
        }
    }

    #[test]
    fn enabling_appends_to_the_existing_list() {
        assert_eq!(with_extension("['a@x', 'b@y']", UUID).unwrap(), format!("['a@x', 'b@y', '{UUID}']"));
        assert_eq!(with_extension("@as []", UUID).unwrap(), format!("['{UUID}']"));
        assert_eq!(with_extension("[]", UUID).unwrap(), format!("['{UUID}']"));
    }

    #[test]
    fn an_enabled_extension_is_left_alone() {
        assert_eq!(with_extension(&format!("['a@x', '{UUID}']"), UUID), None);
    }

    #[test]
    fn disabling_removes_only_our_entry() {
        assert_eq!(without_extension(&format!("['a@x', '{UUID}', 'b@y']"), UUID).unwrap(), "['a@x', 'b@y']");
        assert_eq!(without_extension(&format!("['{UUID}']"), UUID).unwrap(), "[]");
        assert_eq!(without_extension("['a@x']", UUID), None);
        assert_eq!(without_extension("@as []", UUID), None);
    }

    #[test]
    fn files_are_written_into_the_extension_directory() {
        let dir = std::env::temp_dir().join(format!("kairos-gnome-ext-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_files(&dir, Variant::Legacy).unwrap();
        assert_eq!(fs::read_to_string(dir.join("extension.js")).unwrap(), LEGACY_EXTENSION);
        assert_eq!(fs::read_to_string(dir.join("metadata.json")).unwrap(), LEGACY_METADATA);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_extension_on_disk_is_brought_up_to_date_once() {
        let dir = std::env::temp_dir().join(format!("kairos-gnome-refresh-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("extension.js"), "// 0.1.21").unwrap();
        fs::write(dir.join("metadata.json"), MODERN_METADATA).unwrap();
        assert!(refresh_files(&dir, Variant::Modern).unwrap());
        assert_eq!(fs::read_to_string(dir.join("extension.js")).unwrap(), MODERN_EXTENSION);
        assert!(!refresh_files(&dir, Variant::Modern).unwrap(), "nothing to do the second time");
        let _ = fs::remove_dir_all(&dir);
    }
}
