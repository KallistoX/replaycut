//! Read-only look at the OBS Studio configuration on this machine: which
//! profiles exist, which one is current, where each records and in what
//! container. Used by the setup wizard to suggest the recording folder.
//! Nothing here writes to OBS.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    pub current: bool,
    /// Recording folder of the active output mode (Simple or Advanced).
    pub rec_path: Option<String>,
    /// Container format (`mkv`, `mp4`, ...), when set.
    pub format: Option<String>,
    /// `Simple` or `Advanced`.
    pub mode: String,
}

/// `%APPDATA%\obs-studio`.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("obs-studio"))
}

/// A tiny INI reader: `[section]` and `key=value`, OBS escapes backslashes.
fn parse_ini(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.entry(section.clone())
                .or_default()
                .insert(k.trim().to_string(), v.trim().replace("\\\\", "\\"));
        }
    }
    out
}

fn read_ini(path: &Path) -> BTreeMap<String, BTreeMap<String, String>> {
    std::fs::read_to_string(path)
        .map(|t| parse_ini(&t))
        .unwrap_or_default()
}

/// The profile directory name OBS currently uses (`user.ini` since OBS 31,
/// `global.ini` before).
fn current_profile_dir(config: &Path) -> Option<String> {
    for file in ["user.ini", "global.ini"] {
        let ini = read_ini(&config.join(file));
        if let Some(dir) = ini
            .get("Basic")
            .and_then(|b| b.get("ProfileDir").or_else(|| b.get("Profile")))
        {
            return Some(dir.clone());
        }
    }
    None
}

fn profile_from_ini(
    dir_name: &str,
    ini: &BTreeMap<String, BTreeMap<String, String>>,
    current: bool,
) -> Profile {
    let get = |section: &str, key: &str| ini.get(section).and_then(|s| s.get(key)).cloned();
    let mode = get("Output", "Mode").unwrap_or_else(|| "Simple".into());
    let (rec_path, format) = if mode.eq_ignore_ascii_case("Advanced") {
        (
            get("AdvOut", "RecFilePath"),
            get("AdvOut", "RecFormat2").or_else(|| get("AdvOut", "RecFormat")),
        )
    } else {
        (
            get("SimpleOutput", "FilePath"),
            get("SimpleOutput", "RecFormat2").or_else(|| get("SimpleOutput", "RecFormat")),
        )
    };
    Profile {
        name: get("General", "Name").unwrap_or_else(|| dir_name.to_string()),
        current,
        rec_path: rec_path.filter(|p| !p.is_empty()),
        format: format.map(|f| f.to_ascii_lowercase()),
        mode,
    }
}

/// Every profile under `<config>\basic\profiles`, current one first.
pub fn profiles_in(config: &Path) -> Vec<Profile> {
    let current = current_profile_dir(config);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(config.join("basic").join("profiles")) else {
        return out;
    };
    for e in entries.flatten() {
        let ini_path = e.path().join("basic.ini");
        if !ini_path.is_file() {
            continue;
        }
        let dir_name = e.file_name().to_string_lossy().to_string();
        let is_current = current.as_deref() == Some(dir_name.as_str());
        out.push(profile_from_ini(
            &dir_name,
            &read_ini(&ini_path),
            is_current,
        ));
    }
    out.sort_by(|a, b| b.current.cmp(&a.current).then(a.name.cmp(&b.name)));
    out
}

pub fn profiles() -> Vec<Profile> {
    config_dir().map(|d| profiles_in(&d)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_profiles_and_the_current_one() {
        let dir = std::env::temp_dir().join(format!("rc-obs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let profiles_dir = dir.join("basic").join("profiles");
        std::fs::create_dir_all(profiles_dir.join("Gaming")).unwrap();
        std::fs::create_dir_all(profiles_dir.join("Untitled")).unwrap();
        std::fs::write(
            dir.join("user.ini"),
            "[Basic]\nProfile=Gaming\nProfileDir=Gaming\n",
        )
        .unwrap();
        std::fs::write(
            profiles_dir.join("Gaming").join("basic.ini"),
            "[General]\nName=Gaming\n[Output]\nMode=Advanced\n[AdvOut]\nRecFilePath=C:\\\\Users\\\\you\\\\Videos\\\\Clips\nRecFormat2=mkv\n[SimpleOutput]\nFilePath=C:\\\\Other\n",
        )
        .unwrap();
        std::fs::write(
            profiles_dir.join("Untitled").join("basic.ini"),
            "\u{feff}[General]\nName=Untitled\n[SimpleOutput]\nFilePath=C:\\\\Users\\\\you\\\\Videos\nRecFormat2=mp4\n",
        )
        .unwrap();
        let list = profiles_in(&dir);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Gaming");
        assert!(list[0].current);
        assert_eq!(list[0].mode, "Advanced");
        assert_eq!(
            list[0].rec_path.as_deref(),
            Some("C:\\Users\\you\\Videos\\Clips")
        );
        assert_eq!(list[0].format.as_deref(), Some("mkv"));
        assert_eq!(list[1].name, "Untitled");
        assert!(!list[1].current);
        assert_eq!(list[1].rec_path.as_deref(), Some("C:\\Users\\you\\Videos"));
        assert_eq!(list[1].format.as_deref(), Some("mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_config_is_empty() {
        assert!(profiles_in(Path::new("Z:\\does-not-exist")).is_empty());
    }
}
