use crate::config::Config;
use crate::convert::{convert_image, image_size, same_format, within_limits, OutputFormat};
use crate::messages::UiEvent;
use crate::rename::{clean_prefix, is_image_name, sequence_number, SequenceBook};
use crate::winutil::{delete_file, exclusively_readable, move_file};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn run(
    files: Receiver<PathBuf>,
    settings: Arc<Mutex<Config>>,
    book: Arc<Mutex<SequenceBook>>,
    ui: Sender<UiEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut saved: HashMap<String, FileStamp> = HashMap::new();
    while !stop.load(Ordering::SeqCst) {
        let first = match files.recv_timeout(Duration::from_millis(200)) {
            Ok(path) => path,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        for path in unique_paths(first, &files) {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            process_file(&path, &settings, &book, &ui, &stop, &mut saved);
        }
    }
}

fn unique_paths(first: PathBuf, files: &Receiver<PathBuf>) -> Vec<PathBuf> {
    let mut batch = vec![first];
    while let Ok(extra) = files.try_recv() {
        batch.push(extra);
    }
    let mut seen = HashSet::new();
    batch.retain(|path| seen.insert(path_key(path)));
    batch
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: SystemTime,
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok()?,
    })
}

fn already_saved(saved: &HashMap<String, FileStamp>, path: &Path) -> bool {
    match (saved.get(&path_key(path)), file_stamp(path)) {
        (Some(previous), Some(current)) => previous == &current,
        _ => false,
    }
}

fn process_file(
    path: &Path,
    settings: &Mutex<Config>,
    book: &Mutex<SequenceBook>,
    ui: &Sender<UiEvent>,
    stop: &AtomicBool,
    saved: &mut HashMap<String, FileStamp>,
) {
    let config = settings
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let Some(folder) = config.folder.as_deref() else {
        return;
    };
    if !path.is_file() || already_saved(saved, path) {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if !crate::config::same_folder(parent, folder) {
        return;
    }
    let Some(prefix) = clean_prefix(&config.prefix) else {
        return;
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    if file_name.starts_with('~') || file_name.starts_with('.') || !is_image_name(file_name) {
        return;
    }
    if sequence_number(file_name, &prefix).is_some() {
        return;
    }
    let Some(source_ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return;
    };
    let source_ext = source_ext.to_ascii_lowercase();

    let _ = ui.send(UiEvent::Status(format!("Waiting for {file_name} to finish…")));
    if !wait_until_settled(path, stop) {
        if path.exists() && !stop.load(Ordering::SeqCst) {
            let _ = ui.send(UiEvent::Log(format!(
                "{file_name} stayed open, so it was left alone."
            )));
        }
        return;
    }

    let Some(dest_dir) = prepare_destination(&config, folder, ui) else {
        return;
    };
    let target_format = OutputFormat::parse(&config.format);
    let same = same_format(&source_ext, target_format);
    let max_width = if config.resize { config.resize_width } else { 0 };
    let max_height = if config.resize { config.resize_height } else { 0 };
    let stretch = config.resize && config.resize_stretch;
    let convert = config.convert && !same;
    if config.convert && same && max_width == 0 && max_height == 0 {
        let _ = ui.send(UiEvent::Log(format!(
            "{file_name} is already {}, so it was not converted.",
            target_format.label()
        )));
    }
    let needs_encode = convert
        || (max_width > 0 || max_height > 0)
            && !within_limits(image_size(path), max_width, max_height, stretch);
    let width = config.id_width as usize;
    let ext = if config.convert {
        target_format.as_str()
    } else {
        source_ext.as_str()
    };
    let encode_format = if config.convert {
        target_format
    } else {
        OutputFormat::from_extension(&source_ext).unwrap_or(target_format)
    };

    let placed = if needs_encode {
        let action = if convert && (max_width > 0 || max_height > 0) {
            "Converting and resizing"
        } else if convert {
            "Converting"
        } else {
            "Resizing"
        };
        let _ = ui.send(UiEvent::Status(format!(
            "{action} {file_name}…"
        )));
        let temp = dest_dir.join(format!(
            "~auto-renamer-{}-{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match convert_image(
            path,
            &temp,
            encode_format,
            config.quality,
            max_width,
            max_height,
            stretch,
        ) {
            Ok(()) => {
                let destination = claim(book, &dest_dir, &prefix, width, ext);
                match move_file(&temp, &destination) {
                    Ok(()) => finish_success(path, &destination, config.keep_original, ui),
                    Err(err) => {
                        let _ = delete_file(&temp);
                        let _ = ui.send(UiEvent::Log(format!(
                            "Could not put the new file in place: {err}"
                        )));
                        false
                    }
                }
            }
            Err(err) => {
                let _ = delete_file(&temp);
                let _ = ui.send(UiEvent::Log(format!(
                    "Could not process {file_name} ({err}). The original file is used instead."
                )));
                place_original(
                    path,
                    book,
                    &dest_dir,
                    &prefix,
                    width,
                    &source_ext,
                    config.keep_original,
                    ui,
                )
            }
        }
    } else {
        place_original(
            path,
            book,
            &dest_dir,
            &prefix,
            width,
            ext,
            config.keep_original,
            ui,
        )
    };
    if placed {
        if let Some(stamp) = file_stamp(path) {
            saved.insert(path_key(path), stamp);
        }
    }

    let _ = ui.send(UiEvent::Status(format!("Watching {}", folder.display())));
}

fn prepare_destination(config: &Config, watch: &Path, ui: &Sender<UiEvent>) -> Option<PathBuf> {
    let dest = config.destination_folder()?;
    if config.keep_original {
        if let Err(err) = std::fs::create_dir_all(&dest) {
            let _ = ui.send(UiEvent::Log(format!(
                "Could not create the output folder: {err}"
            )));
            return None;
        }
    }
    let _ = watch;
    Some(dest)
}

fn claim(
    book: &Mutex<SequenceBook>,
    folder: &Path,
    prefix: &str,
    width: usize,
    ext: &str,
) -> PathBuf {
    book.lock()
        .unwrap_or_else(|err| err.into_inner())
        .claim(folder, prefix, width, ext)
}

fn finish_success(
    source: &Path,
    destination: &Path,
    keep_original: bool,
    ui: &Sender<UiEvent>,
) -> bool {
    let from = file_label(source);
    let to = file_label(destination);
    let _ = ui.send(UiEvent::Renamed {
        from: from.clone(),
        to: to.clone(),
    });
    if keep_original {
        return true;
    }
    if let Err(err) = delete_when_free(source) {
        let _ = ui.send(UiEvent::Log(format!(
            "Saved {to}, but the original could not be removed: {err}"
        )));
    }
    true
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("the file")
        .to_string()
}

fn place_original(
    path: &Path,
    book: &Mutex<SequenceBook>,
    folder: &Path,
    prefix: &str,
    width: usize,
    ext: &str,
    keep_original: bool,
    ui: &Sender<UiEvent>,
) -> bool {
    let destination = claim(book, folder, prefix, width, ext);
    let from = file_label(path);
    let to = file_label(&destination);
    if keep_original {
        return match std::fs::copy(path, &destination) {
            Ok(_) => {
                let _ = ui.send(UiEvent::Renamed { from, to });
                true
            }
            Err(err) => {
                let _ = ui.send(UiEvent::Log(format!("Could not copy {to}: {err}")));
                false
            }
        };
    }
    match move_file(path, &destination) {
        Ok(()) => {
            let _ = ui.send(UiEvent::Renamed { from, to });
            true
        }
        Err(err) => {
            let _ = ui.send(UiEvent::Log(format!("Could not rename {from}: {err}")));
            false
        }
    }
}

fn wait_until_settled(path: &Path, stop: &AtomicBool) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(120) {
        if stop.load(Ordering::SeqCst) || !path.exists() {
            return false;
        }
        if exclusively_readable(path) {
            let size = std::fs::metadata(path).ok().map(|meta| meta.len());
            if size.unwrap_or(0) > 0 {
                std::thread::sleep(Duration::from_millis(600));
                if stop.load(Ordering::SeqCst) || !path.exists() {
                    return false;
                }
                let again = std::fs::metadata(path).ok().map(|meta| meta.len());
                if exclusively_readable(path) && again == size {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_finished_file_is_not_saved_again() {
        let dir = std::env::temp_dir().join(format!(
            "auto-renamer-once-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, b"png").unwrap();
        let mut saved = HashMap::new();
        assert!(!already_saved(&saved, &path));
        saved.insert(path_key(&path), file_stamp(&path).unwrap());
        assert!(already_saved(&saved, &path));
        std::fs::write(&path, b"png-replaced").unwrap();
        assert!(!already_saved(&saved, &path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queued_notices_for_one_path_collapse() {
        let (tx, rx) = mpsc::channel();
        let first = PathBuf::from(r"K:\cap\shot.png");
        tx.send(PathBuf::from(r"K:\cap\shot.png")).unwrap();
        tx.send(PathBuf::from(r"K:\cap\other.png")).unwrap();
        tx.send(PathBuf::from(r"K:\cap\SHOT.png")).unwrap();
        let batch = unique_paths(first, &rx);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], PathBuf::from(r"K:\cap\shot.png"));
        assert_eq!(batch[1], PathBuf::from(r"K:\cap\other.png"));
    }
}

fn delete_when_free(path: &Path) -> Result<(), String> {
    let mut last = String::from("the file is still open");
    for _ in 0..8 {
        if !path.exists() {
            return Ok(());
        }
        if exclusively_readable(path) {
            return delete_file(path);
        }
        last = "the file is still open".into();
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(last)
}
