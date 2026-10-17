//! `%APPDATA%\Auto Renamer\config.json`.
//! `next_id` is kept in memory. The destination folder is listed when the
//! series key changes and when the user resets the id, not on each file.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub folder: Option<PathBuf>,
    #[serde(default)]
    pub prefix: String,
    #[serde(default = "default_width")]
    pub id_width: u8,
    #[serde(default)]
    pub convert: bool,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_quality")]
    pub quality: u8,
    /// Next id. With `manual_next`, restart does not rescan to max+1.
    #[serde(default)]
    pub next_id: u32,
    #[serde(default)]
    pub manual_next: bool,
    /// Copy into `output_folder` and leave the source file.
    #[serde(default)]
    pub keep_original: bool,
    #[serde(default = "default_output")]
    pub output_folder: String,
    #[serde(default)]
    pub resize: bool,
    #[serde(default = "default_resize_width")]
    pub resize_width: u32,
    #[serde(default = "default_resize_height")]
    pub resize_height: u32,
    /// `true`: scale to the exact box. `false`: shrink inside the box, keep aspect.
    #[serde(default)]
    pub resize_stretch: bool,
    #[serde(default)]
    pub window_x: Option<i32>,
    #[serde(default)]
    pub window_y: Option<i32>,
    #[serde(default)]
    pub window_width: Option<u32>,
    #[serde(default)]
    pub window_height: Option<u32>,
    /// Mirrors the HKCU Run value written by `set_start_with_windows`.
    #[serde(default)]
    pub start_with_windows: bool,
}

fn default_output() -> String {
    "renamed".into()
}

fn default_resize_width() -> u32 {
    1920
}

fn default_resize_height() -> u32 {
    1080
}

fn default_width() -> u8 {
    5
}

fn default_format() -> String {
    "png".into()
}

fn default_quality() -> u8 {
    90
}

impl Default for Config {
    fn default() -> Self {
        Self {
            folder: None,
            prefix: String::new(),
            id_width: default_width(),
            convert: false,
            format: default_format(),
            quality: default_quality(),
            next_id: 0,
            manual_next: false,
            keep_original: false,
            output_folder: default_output(),
            resize: false,
            resize_width: default_resize_width(),
            resize_height: default_resize_height(),
            resize_stretch: false,
            window_x: None,
            window_y: None,
            window_width: None,
            window_height: None,
            start_with_windows: false,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(config_path()) else {
            return Self::default();
        };
        let mut config: Self = serde_json::from_str(&text).unwrap_or_default();
        config.normalize_in_place();
        config
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn normalized(&self) -> Self {
        let mut copy = self.clone();
        copy.normalize_in_place();
        copy
    }

    pub(crate) fn normalize_in_place(&mut self) {
        self.id_width = self.id_width.clamp(1, 10);
        self.quality = self.quality.clamp(1, 100);
        self.resize_width = self.resize_width.clamp(1, 20_000);
        self.resize_height = self.resize_height.clamp(1, 20_000);
        if self.output_folder.trim().is_empty() {
            self.output_folder = default_output();
        }
    }

    /// Destination for the renamed file. With `keep_original`, that is `output_folder`.
    pub fn destination_folder(&self) -> Option<PathBuf> {
        let watch = self.folder.as_ref()?;
        if self.keep_original {
            Some(output_folder_path(watch, &self.output_folder))
        } else {
            Some(watch.clone())
        }
    }
}

/// Relative names resolve under `watch`. A `..` segment falls back to `watch/renamed`.
/// An absolute path is used as given.
pub fn output_folder_path(watch: &Path, raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return watch.join("renamed");
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return watch.join("renamed");
    }
    watch.join(path)
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Auto Renamer")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_output_stays_inside_the_watch_folder() {
        let watch = Path::new(r"D:\renders");
        assert_eq!(
            output_folder_path(watch, "renamed"),
            PathBuf::from(r"D:\renders\renamed")
        );
        assert_eq!(
            output_folder_path(watch, r"..\outside"),
            PathBuf::from(r"D:\renders\renamed")
        );
        assert_eq!(
            output_folder_path(watch, r"E:\out"),
            PathBuf::from(r"E:\out")
        );
    }
}

pub fn same_folder(a: &Path, b: &Path) -> bool {
    a == b
        || a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
}
