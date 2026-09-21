//! The machines this client has connected to, most recent first, kept in
//! `$XDG_CONFIG_HOME/gliff/config.toml` so the address bar can offer them again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many machines the address bar remembers.
pub const MAX_RECENT: usize = 6;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default, rename = "recent_machines")]
    pub recent: Vec<String>,
}

impl Config {
    /// Where the client keeps its config: `~/.config/gliff/config.toml`.
    pub fn default_path() -> PathBuf {
        gtk4::glib::user_config_dir()
            .join("gliff")
            .join("config.toml")
    }

    /// Read the config, or start empty when there is none yet. A file that
    /// cannot be parsed is logged and treated as empty, so a hand edit gone
    /// wrong never stops the client from starting.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read config");
                return Self::default();
            }
        };
        match Self::parse(&text) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot parse config");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(text)?;
        config.recent.retain(|m| !m.trim().is_empty());
        config.recent.truncate(MAX_RECENT);
        Ok(config)
    }

    /// Move `machine` to the top of the list, adding it if it is new, and
    /// drop the oldest entry beyond `MAX_RECENT`.
    pub fn touch(&mut self, machine: &str) {
        let machine = machine.trim();
        if machine.is_empty() {
            return;
        }
        self.recent.retain(|m| m != machine);
        self.recent.insert(0, machine.to_string());
        self.recent.truncate(MAX_RECENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_moves_to_top_and_caps_the_list() {
        let mut c = Config::default();
        for i in 0..MAX_RECENT + 3 {
            c.touch(&format!("host{i}"));
        }
        assert_eq!(c.recent.len(), MAX_RECENT);
        assert_eq!(c.recent[0], format!("host{}", MAX_RECENT + 2));
        assert!(!c.recent.contains(&"host0".to_string()));

        c.touch("host5");
        assert_eq!(c.recent[0], "host5");
        assert_eq!(c.recent.iter().filter(|m| *m == "host5").count(), 1);
        assert_eq!(c.recent.len(), MAX_RECENT);
    }

    #[test]
    fn touch_trims_and_ignores_blank() {
        let mut c = Config::default();
        c.touch("  ");
        c.touch(" user@host ");
        assert_eq!(c.recent, vec!["user@host"]);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config::default();
        c.touch("b");
        c.touch("a");
        let text = toml::to_string(&c).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), c);
    }

    #[test]
    fn parse_tolerates_missing_key_and_drops_blanks() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let c = Config::parse("recent_machines = [\"a\", \"\", \"b\"]").unwrap();
        assert_eq!(c.recent, vec!["a", "b"]);
        assert!(Config::parse("recent_machines = 3").is_err());
    }

    #[test]
    fn load_and_save_files() {
        let dir = std::env::temp_dir().join(format!("gliff-recent-{}", std::process::id()));
        let path = dir.join("nested").join("config.toml");
        assert_eq!(Config::load(&path), Config::default());

        let mut c = Config::default();
        c.touch("user@host");
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);

        std::fs::write(&path, "not toml [").unwrap();
        assert_eq!(Config::load(&path), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
