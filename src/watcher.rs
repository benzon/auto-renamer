use crate::messages::UiEvent;
use crate::rename::is_image_name;
use crate::winutil::open_directory;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING};
use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{
    ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

pub struct FolderWatch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    folder: Option<PathBuf>,
}

impl FolderWatch {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            folder: None,
        }
    }

    pub fn set_folder(
        &mut self,
        folder: Option<PathBuf>,
        files: &Sender<PathBuf>,
        ui: &Sender<UiEvent>,
    ) {
        let unchanged = match (&self.folder, &folder) {
            (Some(current), Some(next)) => {
                current
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&next.to_string_lossy())
                    && self.thread.is_some()
            }
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        self.stop_thread();
        self.folder = folder.clone();
        let Some(folder) = folder else {
            return;
        };
        if !folder.is_dir() {
            let _ = ui.send(UiEvent::Log(format!(
                "Folder does not exist: {}",
                folder.display()
            )));
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Arc::clone(&stop);
        let files = files.clone();
        let ui = ui.clone();
        self.thread = Some(std::thread::spawn(move || {
            watch_directory(folder, files, ui, stop)
        }));
    }

    pub fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FolderWatch {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

fn watch_directory(
    folder: PathBuf,
    files: Sender<PathBuf>,
    ui: Sender<UiEvent>,
    stop: Arc<AtomicBool>,
) {
    let handle = match open_directory(&folder) {
        Ok(handle) => handle,
        Err(err) => {
            let _ = ui.send(UiEvent::Log(format!(
                "Cannot watch {}: {err}",
                folder.display()
            )));
            return;
        }
    };
    let event = match unsafe { CreateEventW(None, true, false, None) } {
        Ok(event) => event,
        Err(err) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            let _ = ui.send(UiEvent::Log(format!("Cannot start watching: {err}")));
            return;
        }
    };

    let mut buffer = vec![0u8; 32 * 1024];
    while !stop.load(Ordering::SeqCst) {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        unsafe {
            let _ = ResetEvent(event);
        }
        let queued = unsafe {
            ReadDirectoryChangesW(
                handle,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE,
                None,
                Some(&mut overlapped),
                None,
            )
        };
        if let Err(err) = queued {
            if err.code() != ERROR_IO_PENDING.into() {
                let _ = ui.send(UiEvent::Log(format!("Watching stopped: {err}")));
                break;
            }
        }

        let mut cancelled = false;
        loop {
            if stop.load(Ordering::SeqCst) {
                unsafe {
                    let _ = CancelIoEx(handle, Some(&overlapped));
                    let _ = WaitForSingleObject(event, 2000);
                }
                cancelled = true;
                break;
            }
            let waited = unsafe { WaitForSingleObject(event, 250) };
            if waited == WAIT_OBJECT_0 {
                break;
            }
            if waited == WAIT_TIMEOUT {
                continue;
            }
            cancelled = true;
            break;
        }
        if cancelled || stop.load(Ordering::SeqCst) {
            break;
        }

        let mut bytes = 0u32;
        let finished = unsafe { GetOverlappedResult(handle, &overlapped, &mut bytes, false) };
        let bytes = bytes as usize;
        if finished.is_err() || bytes == 0 || bytes > buffer.len() {
            continue;
        }
        for (action, name) in parse_notifications(&buffer[..bytes]) {
            if !is_new_image(action, &name) {
                continue;
            }
            let _ = files.send(folder.join(name));
        }
    }

    unsafe {
        let _ = CloseHandle(handle);
        let _ = CloseHandle(event);
    }
}

fn is_new_image(action: u32, name: &str) -> bool {
    if action != FILE_ACTION_ADDED.0
        && action != FILE_ACTION_MODIFIED.0
        && action != FILE_ACTION_RENAMED_NEW_NAME.0
    {
        return false;
    }
    if name.starts_with('~') || name.starts_with('.') {
        return false;
    }
    is_image_name(name)
}

pub fn parse_notifications(buf: &[u8]) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    let mut offset = 0usize;
    while offset + 12 <= buf.len() {
        let next = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(buf[offset + 4..offset + 8].try_into().unwrap());
        let name_len =
            u32::from_le_bytes(buf[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let name_end = offset + 12 + name_len;
        if name_len % 2 != 0 || name_end > buf.len() {
            break;
        }
        let mut units = Vec::with_capacity(name_len / 2);
        for chunk in buf[offset + 12..name_end].chunks_exact(2) {
            units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        if let Some(0) = units.last().copied() {
            units.pop();
        }
        found.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            break;
        }
        if next < 12 || offset + next <= offset {
            break;
        }
        offset += next;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(action: u32, name: &str, more: bool) -> Vec<u8> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        let name_bytes = wide.len() * 2;
        let size = (12 + name_bytes + 3) & !3;
        let mut buf = vec![0u8; size];
        let next = if more { size as u32 } else { 0 };
        buf[0..4].copy_from_slice(&next.to_le_bytes());
        buf[4..8].copy_from_slice(&action.to_le_bytes());
        buf[8..12].copy_from_slice(&(name_bytes as u32).to_le_bytes());
        for (index, unit) in wide.iter().enumerate() {
            buf[12 + index * 2..12 + index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        buf
    }

    #[test]
    fn parses_added_and_renamed_image_names() {
        let mut buf = entry(1, "frame.png", true);
        buf.extend(entry(5, "final.webp", false));
        let parsed = parse_notifications(&buf);
        assert_eq!(
            parsed,
            vec![(1, "frame.png".to_string()), (5, "final.webp".to_string())]
        );
        assert!(is_new_image(1, "frame.png"));
        assert!(!is_new_image(1, "notes.txt"));
        assert!(!is_new_image(1, "~auto-renamer-1.tmp"));
        assert!(!is_new_image(2, "gone.png"));
    }

    #[test]
    fn reports_a_png_that_appears_after_watching_starts() {
        use std::sync::mpsc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let dir = std::env::temp_dir().join(format!(
            "auto-renamer-watch-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (files_tx, files_rx) = mpsc::channel();
        let (ui_tx, ui_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let watched = dir.clone();
        let thread =
            std::thread::spawn(move || watch_directory(watched, files_tx, ui_tx, stop_thread));

        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(dir.join("frame.png"), b"png").unwrap();
        std::fs::write(dir.join("notes.txt"), b"text").unwrap();

        let found = files_rx.recv_timeout(std::time::Duration::from_secs(3));
        stop.store(true, Ordering::SeqCst);
        let _ = thread.join();
        let logs: Vec<_> = ui_rx.try_iter().collect();
        let _ = std::fs::remove_dir_all(&dir);
        let path = found.unwrap_or_else(|_| panic!("ingen billedhændelse. log: {logs:?}"));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("frame.png")
        );
    }
}
