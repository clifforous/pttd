use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Parser;
use evdev::KeyCode;
use serde::Deserialize;

#[derive(Debug, Parser)]
pub struct Args {
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

impl Args {
    pub fn config_path(&self) -> Result<PathBuf, ConfigError> {
        select_path(
            self.config.as_deref(),
            env::var_os("XDG_CONFIG_HOME").as_deref(),
            env::var_os("HOME").as_deref(),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    pub input: InputConfig,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InputConfig {
    pub devices: Vec<PathBuf>,
    pub ptt_key: KeyCode,
    pub toggle_key: KeyCode,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    input: RawInputConfig,
}

#[derive(Debug, Deserialize)]
struct RawInputConfig {
    devices: Vec<PathBuf>,
    ptt_key: String,
    toggle_key: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    MissingConfigHome,
    Toml(String),
    InvalidKey { field: &'static str, value: String },
    EmptyDevices,
    DuplicateDevice(PathBuf),
    EqualKeys,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingConfigHome => {
                write!(formatter, "neither XDG_CONFIG_HOME nor HOME is set")
            }
            Self::Toml(error) => write!(formatter, "invalid configuration: {error}"),
            Self::InvalidKey { field, value } => {
                write!(formatter, "invalid symbolic key name for {field}: {value}")
            }
            Self::EmptyDevices => write!(formatter, "input.devices must not be empty"),
            Self::DuplicateDevice(path) => {
                write!(formatter, "duplicate input device path: {}", path.display())
            }
            Self::EqualKeys => write!(formatter, "ptt_key and toggle_key must be different"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug)]
pub enum LoadError {
    Read { path: PathBuf, source: io::Error },
    Parse(ConfigError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Parse(source) => source.fmt(formatter),
        }
    }
}

impl std::error::Error for LoadError {}

pub fn load(path: &Path) -> Result<Config, LoadError> {
    let contents = fs::read_to_string(path).map_err(|source| LoadError::Read {
        path: path.to_owned(),
        source,
    })?;
    parse(&contents).map_err(LoadError::Parse)
}

pub fn select_path(
    override_path: Option<&Path>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = override_path {
        return Ok(path.to_owned());
    }

    if let Some(xdg_config_home) = xdg_config_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        return Ok(xdg_config_home.join("pttd/config.toml"));
    }

    home.filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/pttd/config.toml"))
        .ok_or(ConfigError::MissingConfigHome)
}

pub fn parse(contents: &str) -> Result<Config, ConfigError> {
    let raw: RawConfig =
        toml::from_str(contents).map_err(|error| ConfigError::Toml(error.to_string()))?;
    if raw.input.devices.is_empty() {
        return Err(ConfigError::EmptyDevices);
    }
    let mut unique_devices = HashSet::new();
    for device in &raw.input.devices {
        if !unique_devices.insert(device) {
            return Err(ConfigError::DuplicateDevice(device.clone()));
        }
    }

    let ptt_key = parse_key("ptt_key", raw.input.ptt_key)?;
    let toggle_key = parse_key("toggle_key", raw.input.toggle_key)?;

    if ptt_key == toggle_key {
        return Err(ConfigError::EqualKeys);
    }

    Ok(Config {
        input: InputConfig {
            devices: raw.input.devices,
            ptt_key,
            toggle_key,
        },
    })
}

fn parse_key(field: &'static str, value: String) -> Result<KeyCode, ConfigError> {
    value
        .parse()
        .map_err(|_| ConfigError::InvalidKey { field, value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    const VALID: &str = r#"
        [input]
        devices = ["/dev/input/pttd-mouse", "/dev/input/pttd-keyboard"]
        ptt_key = "KEY_F9"
        toggle_key = "KEY_F10"
    "#;

    #[test]
    fn override_path_wins_without_environment() {
        assert_eq!(
            select_path(Some(Path::new("/custom/config.toml")), None, None),
            Ok(PathBuf::from("/custom/config.toml"))
        );
    }

    #[test]
    fn xdg_path_precedes_home_fallback() {
        assert_eq!(
            select_path(
                None,
                Some(OsStr::new("/xdg")),
                Some(OsStr::new("/home/user"))
            ),
            Ok(PathBuf::from("/xdg/pttd/config.toml"))
        );
    }

    #[test]
    fn home_path_is_used_without_xdg() {
        assert_eq!(
            select_path(None, None, Some(OsStr::new("/home/user"))),
            Ok(PathBuf::from("/home/user/.config/pttd/config.toml"))
        );
    }

    #[test]
    fn empty_xdg_uses_home_fallback() {
        assert_eq!(
            select_path(None, Some(OsStr::new("")), Some(OsStr::new("/home/user"))),
            Ok(PathBuf::from("/home/user/.config/pttd/config.toml"))
        );
    }

    #[test]
    fn relative_xdg_uses_home_fallback() {
        assert_eq!(
            select_path(
                None,
                Some(OsStr::new("relative")),
                Some(OsStr::new("/home/user"))
            ),
            Ok(PathBuf::from("/home/user/.config/pttd/config.toml"))
        );
    }

    #[test]
    fn empty_home_is_unavailable() {
        assert_eq!(
            select_path(None, Some(OsStr::new("relative")), Some(OsStr::new(""))),
            Err(ConfigError::MissingConfigHome)
        );
    }

    #[test]
    fn missing_environment_is_reported() {
        assert_eq!(
            select_path(None, None, None),
            Err(ConfigError::MissingConfigHome)
        );
    }

    #[test]
    fn config_override_is_parsed_by_clap() {
        let args = Args::try_parse_from(["pttd", "--config", "/tmp/pttd.toml"]).unwrap();
        assert_eq!(
            select_path(args.config.as_deref(), None, None),
            Ok(PathBuf::from("/tmp/pttd.toml"))
        );
    }

    #[test]
    fn symbolic_keys_are_parsed() {
        let config = parse(VALID).unwrap();
        assert_eq!(
            config.input.devices,
            [
                PathBuf::from("/dev/input/pttd-mouse"),
                PathBuf::from("/dev/input/pttd-keyboard")
            ]
        );
        assert_eq!(config.input.ptt_key, KeyCode::KEY_F9);
        assert_eq!(config.input.toggle_key, KeyCode::KEY_F10);
    }

    #[test]
    fn invalid_symbolic_key_is_rejected() {
        let invalid = VALID.replace("KEY_F9", "NOT_A_KEY");
        assert_eq!(
            parse(&invalid),
            Err(ConfigError::InvalidKey {
                field: "ptt_key",
                value: "NOT_A_KEY".into()
            })
        );
    }

    #[test]
    fn equal_keys_are_rejected() {
        let equal = VALID.replace("KEY_F10", "KEY_F9");
        assert_eq!(parse(&equal), Err(ConfigError::EqualKeys));
    }

    #[test]
    fn empty_device_list_is_rejected() {
        let empty = VALID.replace(
            "[\"/dev/input/pttd-mouse\", \"/dev/input/pttd-keyboard\"]",
            "[]",
        );
        assert_eq!(parse(&empty), Err(ConfigError::EmptyDevices));
    }

    #[test]
    fn exact_duplicate_device_paths_are_rejected() {
        let duplicate = VALID.replace("\"/dev/input/pttd-keyboard\"", "\"/dev/input/pttd-mouse\"");
        assert_eq!(
            parse(&duplicate),
            Err(ConfigError::DuplicateDevice(PathBuf::from(
                "/dev/input/pttd-mouse"
            )))
        );
    }

    #[test]
    fn old_single_device_names_are_not_supported() {
        let old = r#"
            [input]
            device_path = "/dev/input/pttd"
            talk_key = "KEY_F9"
            mode_key = "KEY_F10"
        "#;
        assert!(matches!(parse(old), Err(ConfigError::Toml(_))));
    }
}
