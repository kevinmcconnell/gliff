//! Raw access to the Hyprland IPC socket (`hyprctl` without the binary).
//!
//! Instance discovery works without `HYPRLAND_INSTANCE_SIGNATURE`, so a server
//! spawned by ssh can find the running compositor from `$XDG_RUNTIME_DIR`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("XDG_RUNTIME_DIR is not set")]
    NoRuntimeDir,
    #[error("no Hyprland instance found under {0}")]
    NoInstance(PathBuf),
    #[error("Hyprland instance {0} does not exist")]
    UnknownInstance(String),
    #[error("lock file {0} has no Wayland socket name")]
    BadLockFile(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hyprctl `{command}` failed: {response}")]
    Command { command: String, response: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// A running Hyprland instance.
#[derive(Debug, Clone)]
pub struct Instance {
    pub signature: String,
    pub dir: PathBuf,
}

impl Instance {
    /// Locate an instance. Order: explicit signature, `HYPRLAND_INSTANCE_SIGNATURE`,
    /// then the newest instance directory under `$XDG_RUNTIME_DIR/hypr`.
    pub fn discover(explicit: Option<&str>) -> Result<Self> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::NoRuntimeDir)?;
        Self::discover_in(&runtime_dir, explicit)
    }

    pub fn discover_in(runtime_dir: &Path, explicit: Option<&str>) -> Result<Self> {
        let hypr_dir = runtime_dir.join("hypr");
        let explicit = explicit
            .map(str::to_owned)
            .or_else(|| std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok());
        if let Some(sig) = explicit {
            let dir = hypr_dir.join(&sig);
            if !dir.join(".socket.sock").exists() {
                return Err(Error::UnknownInstance(sig));
            }
            return Ok(Self { signature: sig, dir });
        }
        let mut candidates: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&hypr_dir).map_err(|_| Error::NoInstance(hypr_dir.clone()))? {
            let entry = entry?;
            let dir = entry.path();
            if !dir.join(".socket.sock").exists() {
                continue;
            }
            let modified = entry.metadata()?.modified()?;
            candidates.push((modified, entry.file_name().to_string_lossy().into_owned(), dir));
        }
        candidates.sort();
        let (_, signature, dir) = candidates.pop().ok_or(Error::NoInstance(hypr_dir))?;
        Ok(Self { signature, dir })
    }

    pub fn socket_path(&self) -> PathBuf {
        self.dir.join(".socket.sock")
    }

    /// Wayland socket name (e.g. `wayland-1`) as recorded in `hyprland.lock`.
    pub fn wayland_display(&self) -> Result<String> {
        let lock = self.dir.join("hyprland.lock");
        let text = std::fs::read_to_string(&lock)?;
        text.lines()
            .nth(1)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or(Error::BadLockFile(lock))
    }

    /// Send one command and return the raw response.
    pub fn request(&self, command: &str) -> Result<String> {
        let mut stream = UnixStream::connect(self.socket_path())?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(command.as_bytes())?;
        stream.flush()?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        tracing::trace!(command, response = response.trim(), "hyprctl");
        Ok(response)
    }

    /// Send a command that must answer `ok`.
    pub fn dispatch(&self, command: &str) -> Result<()> {
        let response = self.request(command)?;
        if response.trim() == "ok" {
            Ok(())
        } else {
            Err(Error::Command { command: command.to_owned(), response: response.trim().to_owned() })
        }
    }

    pub fn request_json<T: DeserializeOwned>(&self, command: &str) -> Result<T> {
        let response = self.request(&format!("j/{command}"))?;
        Ok(serde_json::from_str(&response)?)
    }

    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        self.request_json("monitors all")
    }

    pub fn get_option(&self, name: &str) -> Result<OptionValue> {
        self.request_json(&format!("getoption {name}"))
    }

    pub fn create_headless_output(&self, name: &str) -> Result<()> {
        self.dispatch(&format!("output create headless {name}"))
    }

    pub fn remove_output(&self, name: &str) -> Result<()> {
        self.dispatch(&format!("output remove {name}"))
    }

    /// Apply a monitor rule: `name,WxH@hz,position,scale`.
    pub fn set_monitor_mode(&self, name: &str, width: u32, height: u32, hz: u32, scale: f32) -> Result<()> {
        self.dispatch(&format!("keyword monitor {name},{width}x{height}@{hz},auto,{scale}"))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub refresh_rate: f64,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default)]
    pub transform: i32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub disabled: bool,
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptionValue {
    pub option: String,
    #[serde(default)]
    pub set: bool,
    #[serde(default)]
    pub int: Option<i64>,
    #[serde(default)]
    pub float: Option<f64>,
    #[serde(default, rename = "str")]
    pub string: Option<String>,
    #[serde(default, rename = "bool")]
    pub boolean: Option<bool>,
}

impl OptionValue {
    pub fn as_bool(&self) -> Option<bool> {
        self.boolean.or_else(|| self.int.map(|i| i != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_newest_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let hypr = tmp.path().join("hypr");
        for (name, age) in [("old_1_1", 20), ("new_2_2", 1)] {
            let dir = hypr.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(".socket.sock"), "").unwrap();
            std::fs::write(dir.join("hyprland.lock"), format!("123\nwayland-{age}\n")).unwrap();
            let t = std::time::SystemTime::now() - Duration::from_secs(age);
            std::fs::File::open(&dir).unwrap().set_modified(t).unwrap();
        }
        let inst = Instance::discover_in(tmp.path(), None).unwrap();
        assert_eq!(inst.signature, "new_2_2");
        assert_eq!(inst.wayland_display().unwrap(), "wayland-1");
        let explicit = Instance::discover_in(tmp.path(), Some("old_1_1")).unwrap();
        assert_eq!(explicit.signature, "old_1_1");
        assert!(Instance::discover_in(tmp.path(), Some("missing")).is_err());
    }

    #[test]
    fn parses_option_values() {
        let v: OptionValue =
            serde_json::from_str(r#"{"option": "x", "bool": false, "set": false }"#).unwrap();
        assert_eq!(v.as_bool(), Some(false));
        let v: OptionValue = serde_json::from_str(r#"{"option": "x", "str": "us", "set": true }"#).unwrap();
        assert_eq!(v.string.as_deref(), Some("us"));
    }
}
