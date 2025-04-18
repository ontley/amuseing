use std::{
    fs,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

use crate::playback::Playlist;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct Playlists {
    playlists: Vec<Playlist>
}

impl Deref for Playlists {
    type Target = Vec<Playlist>;
    fn deref(&self) -> &Self::Target {
        &self.playlists
    }
}

impl DerefMut for Playlists {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.playlists
    }
}

impl Default for Playlists {
    fn default() -> Self {
        // Try to get the default "Music" folder if it exists on the OS, windows and gnome create one by default.
        #[cfg(target_os = "linux")]
        let path = {
            let home = std::env::var("HOME").expect("$HOME should exist on linux");
            let mut path = PathBuf::from(home);
            path.push("Music");
            path
        };
        #[cfg(target_os = "windows")]
        let path = {
            let home = std::env::var("USERPROFILE").expect("%USERPROFILE% should exist on windows");
            let mut path = PathBuf::from(home);
            path.push("Music");
            path
        };
        let playlists = match Playlist::new(path, "Music".into(), None) {
            Ok(playlist) => vec![playlist],
            Err(_) => Vec::new()
        };
        Self {
            playlists
        }
    }
}

#[derive(Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PlayerConfig {
    pub buffer_size: usize,
    pub volume: f64,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            buffer_size: 2048,
            volume: 0.5,
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConfigInner {
    #[serde(flatten)]
    pub player: PlayerConfig,
    #[serde(rename = "playlist")]
    #[serde(default)]
    pub playlists: Playlists,
}

pub struct Config {
    path: PathBuf,
    inner: ConfigInner,
}

impl Deref for Config {
    type Target = ConfigInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Config {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Config {
    pub fn write(&self) {
        fs::write(&self.path, toml::to_string_pretty(&self.inner).unwrap()).unwrap();
    }
}

impl Default for Config {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        let config_dir = std::env::var("HOME").expect("$HOME should exist on linux");
        #[cfg(target_os = "windows")]
        let config_dir = std::env::var("APPDATA").expect("%APPDDATA% should exist on windows");
        let mut path = PathBuf::from(config_dir);
        #[cfg(target_os = "linux")]
        path.push(".config");
        path.push("amuseing");
        if !path.exists() {
            fs::create_dir(&path).unwrap();
        }
        path.push("config.toml");
        let inner = if path.exists() {
            let toml_str = fs::read_to_string(&path).unwrap();
            toml::from_str(&toml_str).unwrap()
        } else {
            let config = ConfigInner::default();
            fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();
            config
        };
        Self { path, inner }
    }
}
