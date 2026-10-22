//! A profile's Advanced settings, as setup saves them and the agent reads
//! them at start, so the two can't disagree about the file's shape.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::Limits;

/// What a user can adjust under Advanced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub limits: Limits,
    /// Folders tools may send files from. Empty means no file can be sent.
    pub send_roots: Vec<PathBuf>,
}

impl Settings {
    /// The settings saved at `path`, or the defaults when there are none yet.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("unreadable settings file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("can't read {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("relay-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A first start has no settings file and must get the conservative
    /// defaults, with no folder open for sending.
    #[test]
    fn missing_settings_mean_default_limits_and_no_send_folders() {
        let dir = temp_dir();
        let settings = Settings::load(&dir.join("settings.json")).expect("load");
        assert_eq!(settings, Settings::default());
        assert!(settings.send_roots.is_empty());
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// Partial settings keep defaults for the rest, and a misspelled or zero
    /// limit is an error rather than a silently looser account.
    #[test]
    fn settings_fill_gaps_with_defaults_and_refuse_typos_and_zero_rates() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{"limits": {"per_hour": 50}}"#).expect("write");
        let settings = Settings::load(&path).expect("load");
        assert_eq!(
            settings.limits.per_hour,
            NonZeroU32::new(50).expect("nonzero literal")
        );
        assert_eq!(
            settings.limits.burst_per_minute,
            Limits::default().burst_per_minute
        );
        for bad in [
            r#"{"limit": {}}"#,
            r#"{"limits": {"per_hour": 0}}"#,
            r#"{"send_root": []}"#,
        ] {
            std::fs::write(&path, bad).expect("write");
            let loaded = Settings::load(&path);
            assert!(loaded.is_err(), "accepted {bad}: {loaded:?}");
        }
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }
}
