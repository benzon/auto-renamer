use std::path::{Path, PathBuf};
use std::time::Instant;

pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "avif", "gif", "bmp", "tif", "tiff", "heic", "heif", "jxl", "tga",
];

pub fn is_image_name(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() {
        return false;
    }
    let ext = ext.to_ascii_lowercase();
    IMAGE_EXTENSIONS.iter().any(|known| *known == ext)
}

pub fn clean_prefix(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 80 || trimmed.ends_with('.') {
        return None;
    }
    if trimmed.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
        return None;
    }
    Some(trimmed.to_string())
}

pub fn sequence_name(prefix: &str, id: u32, width: usize, ext: &str) -> String {
    let width = width.clamp(1, 10);
    format!("{prefix}_{id:0width$}.{ext}")
}

pub fn sequence_number(file_name: &str, prefix: &str) -> Option<u32> {
    let head = format!("{prefix}_");
    let (start, rest) = file_name.split_at_checked(head.len())?;
    if !start.eq_ignore_ascii_case(&head) {
        return None;
    }
    let (num, ext) = rest.split_once('.')?;
    if num.is_empty()
        || ext.is_empty()
        || ext.contains('.')
        || !num.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    num.parse().ok()
}

pub fn next_sequence(folder: &Path, prefix: &str) -> u32 {
    let mut max_id = 0u32;
    let Ok(entries) = std::fs::read_dir(folder) else {
        return 1;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(id) = sequence_number(name, prefix) {
            max_id = max_id.max(id);
        }
    }
    max_id.saturating_add(1)
}

#[allow(dead_code)]
pub fn allocate_path(folder: &Path, prefix: &str, width: usize, ext: &str) -> PathBuf {
    let mut id = next_sequence(folder, prefix);
    loop {
        let path = folder.join(sequence_name(prefix, id, width, ext));
        if !path.exists() {
            return path;
        }
        if id == u32::MAX {
            return path;
        }
        id += 1;
    }
}

pub fn first_missing(folder: &Path, prefix: &str) -> u32 {
    let mut used = Vec::new();
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(id) = sequence_number(name, prefix) {
                used.push(id);
            }
        }
    }
    used.sort_unstable();
    used.dedup();
    let mut expect = 1u32;
    for id in used {
        if id > expect {
            return expect;
        }
        if id == expect && expect < u32::MAX {
            expect += 1;
        }
    }
    expect
}

pub fn series_key(folder: Option<&Path>, prefix: &str) -> String {
    let folder = folder
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let prefix = clean_prefix(prefix).unwrap_or_default().to_lowercase();
    format!("{folder}|{prefix}")
}

pub struct SequenceBook {
    key: String,
    next_id: u32,
    manual: bool,
    last_claim: Option<Instant>,
}

impl SequenceBook {
    pub fn new() -> Self {
        Self {
            key: String::new(),
            next_id: 1,
            manual: false,
            last_claim: None,
        }
    }

    pub fn from_config(config: &crate::config::Config) -> Self {
        let mut book = Self::new();
        let folder = config.destination_folder();
        let prefix = config.prefix.as_str();
        if config.manual_next && config.next_id > 0 {
            book.key = series_key(folder.as_deref(), prefix);
            book.manual = true;
            book.next_id = config.next_id;
            return book;
        }
        book.align(folder.as_deref(), prefix);
        book
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn next_id(&self) -> u32 {
        self.next_id.max(1)
    }

    pub fn is_manual(&self) -> bool {
        self.manual
    }

    pub fn settled(&self, quiet: std::time::Duration) -> bool {
        self.last_claim
            .map(|instant| instant.elapsed() >= quiet)
            .unwrap_or(true)
    }

    pub fn align(&mut self, folder: Option<&Path>, prefix: &str) -> bool {
        let key = series_key(folder, prefix);
        if key == self.key {
            return false;
        }
        self.manual = false;
        self.key = key;
        let Some(prefix) = clean_prefix(prefix) else {
            self.next_id = 1;
            return true;
        };
        self.next_id = folder
            .filter(|folder| folder.is_dir())
            .map(|folder| next_sequence(folder, &prefix))
            .unwrap_or(1);
        true
    }

    pub fn set_manual(&mut self, id: u32) {
        self.manual = true;
        self.next_id = id.max(1);
        self.last_claim = None;
    }

    pub fn reset(&mut self, folder: Option<&Path>, prefix: &str) -> u32 {
        self.key = series_key(folder, prefix);
        self.manual = true;
        self.last_claim = None;
        self.next_id = match (folder, clean_prefix(prefix)) {
            (Some(folder), Some(prefix)) if folder.is_dir() => first_missing(folder, &prefix),
            _ => 1,
        };
        self.next_id
    }

    pub fn claim(&mut self, folder: &Path, prefix: &str, width: usize, ext: &str) -> PathBuf {
        self.align(Some(folder), prefix);
        loop {
            let id = self.next_id.max(1);
            if self.next_id < u32::MAX {
                self.next_id += 1;
            }
            self.last_claim = Some(Instant::now());
            let path = folder.join(sequence_name(prefix, id, width, ext));
            if !path.exists() || id == u32::MAX {
                return path;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_numbered_names_case_insensitively() {
        assert_eq!(
            sequence_number("screenshot_00001.png", "screenshot"),
            Some(1)
        );
        assert_eq!(
            sequence_number("Screenshot_00009.PNG", "screenshot"),
            Some(9)
        );
        assert_eq!(
            sequence_number("screenshot_00001.jpeg", "screenshot"),
            Some(1)
        );
        assert_eq!(sequence_number("screenshot_2.png", "screenshot"), Some(2));
        assert_eq!(sequence_number("my_shot_00004.webp", "my_shot"), Some(4));
    }

    #[test]
    fn rejects_names_that_are_not_our_sequence() {
        assert_eq!(sequence_number("screenshot2.png", "screenshot"), None);
        assert_eq!(sequence_number("othershot_00001.png", "screenshot"), None);
        assert_eq!(sequence_number("screenshot_00001", "screenshot"), None);
        assert_eq!(
            sequence_number("screenshot_00001.png.bak", "screenshot"),
            None
        );
        assert_eq!(sequence_number("screenshot_.png", "screenshot"), None);
    }

    #[test]
    fn continues_from_the_highest_existing_number() {
        let dir = std::env::temp_dir().join(format!(
            "auto-renamer-seq-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("screenshot_00001.png"), b"a").unwrap();
        fs::write(dir.join("screenshot_00003.jpg"), b"b").unwrap();
        fs::write(dir.join("notes.txt"), b"c").unwrap();
        assert_eq!(next_sequence(&dir, "screenshot"), 4);
        assert_eq!(
            allocate_path(&dir, "screenshot", 4, "png"),
            dir.join("screenshot_0004.png")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pads_to_the_requested_width_without_cutting_larger_ids() {
        assert_eq!(
            sequence_name("screenshot", 1, 4, "png"),
            "screenshot_0001.png"
        );
        assert_eq!(
            sequence_name("screenshot", 1, 5, "png"),
            "screenshot_00001.png"
        );
        assert_eq!(
            sequence_name("screenshot", 100000, 4, "png"),
            "screenshot_100000.png"
        );
    }

    #[test]
    fn prefix_rejects_windows_reserved_characters() {
        assert_eq!(
            clean_prefix("  screenshot  "),
            Some("screenshot".to_string())
        );
        assert_eq!(clean_prefix(""), None);
        assert_eq!(clean_prefix("shot/cut"), None);
        assert_eq!(clean_prefix("shot."), None);
    }

    #[test]
    fn first_missing_fills_the_lowest_gap() {
        let dir = std::env::temp_dir().join(format!(
            "auto-renamer-gap-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("shot_0001.png"), b"a").unwrap();
        fs::write(dir.join("shot_0003.png"), b"c").unwrap();
        assert_eq!(first_missing(&dir, "shot"), 2);
        assert_eq!(next_sequence(&dir, "shot"), 4);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_counts_forward_without_reading_the_folder_again() {
        let dir = std::env::temp_dir().join(format!(
            "auto-renamer-mem-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut book = SequenceBook::new();
        let first = book.claim(&dir, "shot", 4, "png");
        let second = book.claim(&dir, "shot", 4, "png");
        assert_eq!(first, dir.join("shot_0001.png"));
        assert_eq!(second, dir.join("shot_0002.png"));
        assert_eq!(book.reset(Some(&dir), "shot"), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
