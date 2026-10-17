#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod config;
mod convert;
mod messages;
mod rename;
mod update;
mod watcher;
mod winutil;
mod worker;

slint::include_modules!();

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use config::Config;
use convert::{ensure_ffmpeg, OutputFormat};
use messages::UiEvent;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rename::{clean_prefix, sequence_name, series_key, SequenceBook};
use slint::{ComponentHandle, PhysicalPosition, SharedString};
use watcher::FolderWatch;
use windows::Win32::Foundation::HWND;
use winutil::{
    ask_yes_no, claim_single_instance, hide_window, open_folder, open_url, pick_folder,
    placement_visible, set_start_with_windows, show_window, strip_maximize, ComGuard, InstanceLock,
    TrayIcon, REQUEST_QUIT, TRAY_READY,
};

const SETTINGS_SAVE: Duration = Duration::from_millis(600);
const WINDOW_SAVE: Duration = Duration::from_millis(500);
const PREVIEW_SETTLE: Duration = Duration::from_millis(800);
const INDEX_SAVE: Duration = Duration::from_secs(60);
const LOG_KEEP: Duration = Duration::from_secs(12 * 60 * 60);
const LOG_MAX: usize = 200;

fn main() {
    if let Some(pid) = update::restart_after_arg() {
        update::wait_for_exit(pid);
    }
    update::remove_previous_exe();
    if let Err(err) = run() {
        let text: Vec<u16> = format!("Auto Renamer could not start.\n{err}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                windows::core::PCWSTR(text.as_ptr()),
                windows::core::w!("Auto Renamer"),
                windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR
                    | windows::Win32::UI::WindowsAndMessaging::MB_OK,
            );
        }
    }
}

fn run() -> Result<(), slint::PlatformError> {
    let _com = ComGuard::init();
    let _instance = match claim_single_instance() {
        InstanceLock::AlreadyRunning => return Ok(()),
        InstanceLock::Primary(handle) => handle,
    };

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = config::config_dir().join("panic.log");
        let _ = std::fs::create_dir_all(config::config_dir());
        let _ = std::fs::write(&path, info.to_string());
        previous_hook(info);
    }));

    let config = Config::load().normalized();
    let _ = set_start_with_windows(config.start_with_windows);
    let book = Arc::new(Mutex::new(SequenceBook::from_config(&config)));
    let shown_id = lock(&book).next_id();
    let settings = Arc::new(Mutex::new(config.clone()));
    let (file_tx, file_rx) = mpsc::channel::<PathBuf>();
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let worker_stop = Arc::new(AtomicBool::new(false));
    {
        let settings = Arc::clone(&settings);
        let book = Arc::clone(&book);
        let ui_tx = ui_tx.clone();
        let worker_stop = Arc::clone(&worker_stop);
        std::thread::spawn(move || worker::run(file_rx, settings, book, ui_tx, worker_stop));
    }

    let ui = AutoRenamer::new()?;
    apply_config(&ui, &config, shown_id);
    let saved_place = restore_window(&ui, &config);
    let ready_at_start = is_ready(&config);

    let watch = Arc::new(Mutex::new(FolderWatch::new()));
    if let Some(folder) = config.folder.clone().filter(|folder| folder.is_dir()) {
        if clean_prefix(&config.prefix).is_some() {
            watch.lock().unwrap().set_folder(Some(folder), &file_tx, &ui_tx);
        }
    }
    if (config.convert || config.resize) && convert::ffmpeg_status().is_none() {
        let tx = ui_tx.clone();
        std::thread::spawn(move || announce_ffmpeg(&tx));
    }

    let lines = Rc::new(RefCell::new(Vec::<LogLine>::new()));

    let tick = Rc::new(RefCell::new(Tick {
        save_dirty: false,
        save_at: Instant::now(),
        reseed_at: None,
        shown_id,
        index_dirty: false,
        index_saved_at: Instant::now(),
        window_seen: None,
        window_moved_at: None,
        chrome_ready: false,
        tray_tries: 0,
        update_prompted: false,
        update_busy: false,
        update_version: String::new(),
        update_url: String::new(),
    }));
    let tray: Rc<RefCell<Option<TrayIcon>>> = Rc::new(RefCell::new(None));
    let hwnd = Rc::new(Cell::new(0isize));
    let events = Rc::new(RefCell::new(ui_rx));

    wire(
        &ui,
        Arc::clone(&settings),
        Arc::clone(&book),
        Arc::clone(&watch),
        file_tx.clone(),
        ui_tx.clone(),
        Rc::clone(&tick),
        Rc::clone(&hwnd),
    );

    ui.window().on_close_requested({
        let hwnd = Rc::clone(&hwnd);
        move || {
            if TRAY_READY.load(Ordering::SeqCst) {
                let raw = hwnd.get();
                if raw != 0 {
                    hide_window(HWND(raw as *mut std::ffi::c_void));
                }
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        }
    });
    ui.show()?;
    {
        let tx = ui_tx.clone();
        std::thread::spawn(move || {
            if let Some(offer) = update::available() {
                let _ = tx.send(UiEvent::UpdateAvailable {
                    version: offer.version,
                    url: offer.url,
                });
            }
        });
    }

    let ui_weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), {
        let settings = Arc::clone(&settings);
        let book = Arc::clone(&book);
        let tick = Rc::clone(&tick);
        let lines = Rc::clone(&lines);
        let events = Rc::clone(&events);
        let tray = Rc::clone(&tray);
        let hwnd = Rc::clone(&hwnd);
        let ui_tx = ui_tx.clone();
        let home_ready = ready_at_start;
        let home_pos = saved_place;
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            if REQUEST_QUIT.load(Ordering::SeqCst) {
                flush_pending(&ui, &settings, &book, &tick);
                let _ = slint::quit_event_loop();
                return;
            }
            ensure_chrome(&ui, &tick, &tray, &hwnd, home_ready, home_pos);
            let offer = drain_events(&ui, &events, &lines, &tick);
            if let Some((version, url)) = offer {
                offer_update(&ui, &tick, &hwnd, &ui_tx, version, url);
            }
            batch(&ui, &settings, &book, &tick);
            note_window(&ui, &settings, &book, &tick);
            prune_log(&ui, &lines);
        }
    });

    ui.run()?;
    let _ = timer;
    worker_stop.store(true, Ordering::SeqCst);
    watch.lock().unwrap().stop_thread();
    flush_pending(&ui, &settings, &book, &tick);
    drop(file_tx);
    drop(tray);
    Ok(())
}

struct LogLine {
    at: Instant,
    original: String,
    renamed: String,
    time: String,
    counts: bool,
}

thread_local! {
    static APPLYING: Cell<bool> = const { Cell::new(false) };
}

fn with_apply(update: impl FnOnce()) {
    APPLYING.with(|flag| {
        let previous = flag.replace(true);
        update();
        flag.set(previous);
    });
}

fn is_applying() -> bool {
    APPLYING.with(|flag| flag.get())
}

struct Tick {
    save_dirty: bool,
    save_at: Instant,
    reseed_at: Option<Instant>,
    shown_id: u32,
    index_dirty: bool,
    index_saved_at: Instant,
    window_seen: Option<(i32, i32, u32, u32)>,
    window_moved_at: Option<Instant>,
    chrome_ready: bool,
    tray_tries: u8,
    update_prompted: bool,
    update_busy: bool,
    update_version: String,
    update_url: String,
}

fn wire(
    ui: &AutoRenamer,
    settings: Arc<Mutex<Config>>,
    book: Arc<Mutex<SequenceBook>>,
    watch: Arc<Mutex<FolderWatch>>,
    file_tx: mpsc::Sender<PathBuf>,
    ui_tx: mpsc::Sender<UiEvent>,
    tick: Rc<RefCell<Tick>>,
    hwnd: Rc<Cell<isize>>,
) {
    ui.on_edited({
        let ui_weak = ui.as_weak();
        let settings = Arc::clone(&settings);
        let book = Arc::clone(&book);
        let watch = Arc::clone(&watch);
        let file_tx = file_tx.clone();
        let ui_tx = ui_tx.clone();
        let tick = Rc::clone(&tick);
        move || {
            if is_applying() {
                return;
            }
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            commit(
                &ui,
                &settings,
                &book,
                &watch,
                &file_tx,
                &ui_tx,
                &tick,
                false,
            );
        }
    });

    ui.on_choose_folder({
        let ui_weak = ui.as_weak();
        let settings = Arc::clone(&settings);
        let book = Arc::clone(&book);
        let watch = Arc::clone(&watch);
        let file_tx = file_tx.clone();
        let ui_tx = ui_tx.clone();
        let tick = Rc::clone(&tick);
        let hwnd = Rc::clone(&hwnd);
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let current = lock(&settings).folder.clone();
            let Some(folder) = pick_folder(
                HWND(hwnd.get() as *mut std::ffi::c_void),
                current.as_deref(),
                "Select the folder to watch",
            ) else {
                return;
            };
            with_apply(|| {
                ui.set_folder_path(SharedString::from(folder.display().to_string()));
            });
            lock(&settings).folder = Some(folder);
            commit(
                &ui,
                &settings,
                &book,
                &watch,
                &file_tx,
                &ui_tx,
                &tick,
                false,
            );
        }
    });

    ui.on_open_folder({
        let ui_weak = ui.as_weak();
        let settings = Arc::clone(&settings);
        move || {
            if ui_weak.upgrade().is_none() {
                return;
            }
            let Some(folder) = lock(&settings)
                .folder
                .clone()
                .filter(|folder| folder.is_dir())
            else {
                return;
            };
            open_folder(&folder);
        }
    });

    ui.on_open_output_folder({
        let ui_weak = ui.as_weak();
        let settings = Arc::clone(&settings);
        move || {
            if ui_weak.upgrade().is_none() {
                return;
            }
            let config = lock(&settings).clone();
            if !config.keep_original {
                return;
            }
            let Some(folder) = config.destination_folder() else {
                return;
            };
            if !folder.is_dir() {
                let _ = std::fs::create_dir_all(&folder);
            }
            open_folder(&folder);
        }
    });

    ui.on_choose_output_folder({
        let ui_weak = ui.as_weak();
        let settings = Arc::clone(&settings);
        let book = Arc::clone(&book);
        let watch = Arc::clone(&watch);
        let file_tx = file_tx.clone();
        let ui_tx = ui_tx.clone();
        let tick = Rc::clone(&tick);
        let hwnd = Rc::clone(&hwnd);
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let current = lock(&settings).destination_folder();
            let Some(folder) = pick_folder(
                HWND(hwnd.get() as *mut std::ffi::c_void),
                current.as_deref(),
                "Select the folder copies are saved to",
            ) else {
                return;
            };
            with_apply(|| {
                ui.set_copy_to_output_enabled(true);
                ui.set_output_folder_path(SharedString::from(folder.display().to_string()));
            });
            commit(
                &ui,
                &settings,
                &book,
                &watch,
                &file_tx,
                &ui_tx,
                &tick,
                false,
            );
        }
    });

    ui.on_open_github(|| open_url("https://github.com/benzon/auto-renamer"));

    ui.on_install_update({
        let ui_weak = ui.as_weak();
        let tick = Rc::clone(&tick);
        let hwnd = Rc::clone(&hwnd);
        let ui_tx = ui_tx.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let (version, url, busy) = {
                let tick = tick.borrow();
                (tick.update_version.clone(), tick.update_url.clone(), tick.update_busy)
            };
            if busy || url.is_empty() {
                return;
            }
            begin_update(&ui, &tick, &hwnd, &ui_tx, version, url);
        }
    });

    ui.on_reset_id_requested({
        let ui_weak = ui.as_weak();
        let settings = Arc::clone(&settings);
        let book = Arc::clone(&book);
        let watch = Arc::clone(&watch);
        let tick = Rc::clone(&tick);
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            commit(
                &ui,
                &settings,
                &book,
                &watch,
                &file_tx,
                &ui_tx,
                &tick,
                true,
            );
            let config = lock(&settings).clone();
            let id = lock(&book).reset(config.destination_folder().as_deref(), &config.prefix);
            {
                let mut tick = tick.borrow_mut();
                tick.shown_id = id;
                tick.reseed_at = None;
                tick.save_dirty = false;
                tick.index_dirty = false;
                tick.index_saved_at = Instant::now();
            }
            persist(&book, &settings);
            refresh_labels(&ui, &config, id);
        }
    });

}

fn commit(
    ui: &AutoRenamer,
    settings: &Arc<Mutex<Config>>,
    book: &Arc<Mutex<SequenceBook>>,
    watch: &Arc<Mutex<FolderWatch>>,
    file_tx: &mpsc::Sender<PathBuf>,
    ui_tx: &mpsc::Sender<UiEvent>,
    tick: &Rc<RefCell<Tick>>,
    quiet: bool,
) {
    if is_applying() {
        return;
    }
    let previous_startup = lock(settings).start_with_windows;
    with_apply(|| {
        sync_start(ui, book, tick);
        pull(ui, settings);
        let startup = lock(settings).start_with_windows;
        if startup != previous_startup {
            if let Err(err) = set_start_with_windows(startup) {
                let _ = ui_tx.send(UiEvent::Log(format!(
                    "Could not update Windows startup: {err}"
                )));
            }
        }
        commit_rest(ui, settings, book, watch, file_tx, ui_tx, tick, quiet);
    });
}

fn commit_rest(
    ui: &AutoRenamer,
    settings: &Arc<Mutex<Config>>,
    book: &Arc<Mutex<SequenceBook>>,
    watch: &Arc<Mutex<FolderWatch>>,
    file_tx: &mpsc::Sender<PathBuf>,
    ui_tx: &mpsc::Sender<UiEvent>,
    tick: &Rc<RefCell<Tick>>,
    quiet: bool,
) {
    let config = lock(settings).clone();
    let shown = tick.borrow().shown_id;
    refresh_labels(ui, &config, shown);
    if !quiet {
        let key_now = series_key(config.destination_folder().as_deref(), &config.prefix);
        let key_changed = lock(book).key() != key_now;
        let mut tick = tick.borrow_mut();
        tick.save_dirty = true;
        tick.save_at = Instant::now();
        tick.reseed_at = if key_changed { Some(Instant::now()) } else { None };
    }
    let folder = config.folder.clone().filter(|folder| folder.is_dir());
    let prefix_ok = clean_prefix(&config.prefix).is_some();
    watch
        .lock()
        .unwrap()
        .set_folder(folder.filter(|_| prefix_ok), file_tx, ui_tx);
    if (config.convert || config.resize) && convert::ffmpeg_status().is_none() {
        let tx = ui_tx.clone();
        std::thread::spawn(move || announce_ffmpeg(&tx));
    }
    if !is_ready(&config) {
        let _ = ui.window().show();
    }
}

fn sync_start(ui: &AutoRenamer, book: &Arc<Mutex<SequenceBook>>, tick: &Rc<RefCell<Tick>>) {
    let requested = ui.get_start_number().max(1) as u32;
    if lock(book).next_id() == requested {
        return;
    }
    lock(book).set_manual(requested);
    let mut tick = tick.borrow_mut();
    tick.shown_id = requested;
    tick.index_dirty = true;
    tick.reseed_at = None;
}

fn pull(ui: &AutoRenamer, settings: &Mutex<Config>) {
    let mut config = lock(settings);
    config.prefix = ui.get_base_name().to_string();
    config.id_width = ui.get_id_length().clamp(1, 10) as u8;
    config.keep_original = ui.get_copy_to_output_enabled();
    remember_output(&mut config, &ui.get_output_folder_path());
    config.convert = ui.get_convert_enabled();
    config.format = format_code(&ui.get_output_format()).to_string();
    config.quality = ui.get_jpg_quality().round().clamp(1.0, 100.0) as u8;
    config.resize = ui.get_resize_enabled();
    config.resize_width = ui.get_image_width().clamp(1, 20_000) as u32;
    config.resize_height = ui.get_image_height().clamp(1, 20_000) as u32;
    config.resize_stretch = ui.get_aspect_mode() != 0;
    config.start_with_windows = ui.get_start_with_windows();
    config.normalize_in_place();
}

fn batch(
    ui: &AutoRenamer,
    settings: &Arc<Mutex<Config>>,
    book: &Arc<Mutex<SequenceBook>>,
    tick: &Rc<RefCell<Tick>>,
) {
    let (reseed_due, save_due, shown) = {
        let tick = tick.borrow();
        (
            tick.reseed_at
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(400)),
            tick.save_dirty && tick.save_at.elapsed() >= SETTINGS_SAVE,
            tick.shown_id,
        )
    };
    let mut shown = shown;
    if reseed_due {
        let config = lock(settings).clone();
        let id = {
            let mut book = lock(book);
            book.align(config.destination_folder().as_deref(), &config.prefix);
            book.next_id()
        };
        tick.borrow_mut().reseed_at = None;
        if shown != id {
            shown = id;
            let mut tick = tick.borrow_mut();
            tick.shown_id = id;
            tick.index_dirty = true;
        }
        refresh_labels(ui, &config, shown);
    } else if tick.borrow().reseed_at.is_none() {
        let (settled, id) = {
            let book = lock(book);
            (book.settled(PREVIEW_SETTLE), book.next_id())
        };
        if settled && shown != id {
            shown = id;
            let mut tick = tick.borrow_mut();
            tick.shown_id = id;
            tick.index_dirty = true;
            drop(tick);
            let config = lock(settings).clone();
            refresh_labels(ui, &config, shown);
        }
    }
    let index_due = {
        let tick = tick.borrow();
        tick.index_dirty && tick.index_saved_at.elapsed() >= INDEX_SAVE
    };
    if index_due {
        persist(book, settings);
        let mut tick = tick.borrow_mut();
        tick.index_dirty = false;
        tick.index_saved_at = Instant::now();
    }
    if save_due {
        if tick.borrow().reseed_at.is_some() {
            let config = lock(settings).clone();
            let id = {
                let mut book = lock(book);
                book.align(config.destination_folder().as_deref(), &config.prefix);
                book.next_id()
            };
            tick.borrow_mut().reseed_at = None;
            if tick.borrow().shown_id != id {
                tick.borrow_mut().shown_id = id;
                tick.borrow_mut().index_dirty = true;
                refresh_labels(ui, &config, id);
            }
        }
        persist(book, settings);
        let mut tick = tick.borrow_mut();
        tick.save_dirty = false;
        tick.index_dirty = false;
        tick.index_saved_at = Instant::now();
    }
}

fn note_window(
    ui: &AutoRenamer,
    settings: &Arc<Mutex<Config>>,
    book: &Arc<Mutex<SequenceBook>>,
    tick: &Rc<RefCell<Tick>>,
) {
    if !ui.window().is_visible() {
        return;
    }
    let rect = window_rect(ui);
    if rect.0 < -16_000 {
        return;
    }
    let moved_at = {
        let mut tick = tick.borrow_mut();
        if tick.window_seen.is_none() {
            tick.window_seen = Some(rect);
            tick.window_moved_at = None;
            return;
        }
        if tick.window_seen != Some(rect) {
            tick.window_seen = Some(rect);
            tick.window_moved_at = Some(Instant::now());
            return;
        }
        tick.window_moved_at
    };
    let Some(moved_at) = moved_at else {
        return;
    };
    if moved_at.elapsed() < WINDOW_SAVE {
        return;
    }
    tick.borrow_mut().window_moved_at = None;
    let changed = {
        let config = lock(settings);
        config.window_x != Some(rect.0)
            || config.window_y != Some(rect.1)
            || config.window_width != Some(rect.2)
            || config.window_height != Some(rect.3)
    };
    if !changed {
        return;
    }
    {
        let mut config = lock(settings);
        config.window_x = Some(rect.0);
        config.window_y = Some(rect.1);
        config.window_width = Some(rect.2);
        config.window_height = Some(rect.3);
    }
    persist(book, settings);
}

fn ensure_chrome(
    ui: &AutoRenamer,
    tick: &Rc<RefCell<Tick>>,
    tray: &Rc<RefCell<Option<TrayIcon>>>,
    hwnd_slot: &Cell<isize>,
    ready: bool,
    home: Option<(i32, i32)>,
) {
    if tick.borrow().chrome_ready {
        return;
    }
    let Some(hwnd) = native_hwnd(ui) else {
        return;
    };
    strip_maximize(hwnd);
    hwnd_slot.set(hwnd.0 as isize);
    if tray.borrow().is_none() {
        *tray.borrow_mut() = Some(TrayIcon::install(hwnd));
    }
    if let Some(icon) = tray.borrow_mut().as_mut() {
        icon.ensure_added();
    }
    let added = tray.borrow().as_ref().is_some_and(|icon| icon.is_added());
    if !added {
        let tries = {
            let mut tick = tick.borrow_mut();
            tick.tray_tries = tick.tray_tries.saturating_add(1);
            tick.tray_tries
        };
        if tries < 20 {
            return;
        }
    } else if ready {
        hide_window(hwnd);
    }
    let rect = window_rect(ui);
    let (x, y) = home.unwrap_or_else(|| {
        winutil::centered_on_primary(rect.2 as i32, rect.3 as i32)
    });
    let (x, y, _, _) = winutil::fit_on_work_area(x, y, rect.2 as i32, rect.3 as i32);
    ui.window().set_position(PhysicalPosition::new(x, y));
    let mut tick = tick.borrow_mut();
    tick.chrome_ready = true;
    tick.window_moved_at = None;
    tick.window_seen = Some((x, y, rect.2, rect.3));
}

fn flush_pending(
    ui: &AutoRenamer,
    settings: &Arc<Mutex<Config>>,
    book: &Arc<Mutex<SequenceBook>>,
    tick: &Rc<RefCell<Tick>>,
) {
    if ui.window().is_visible() {
        let rect = window_rect(ui);
        if rect.0 > -16_000 {
            let mut config = lock(settings);
            config.window_x = Some(rect.0);
            config.window_y = Some(rect.1);
            config.window_width = Some(rect.2);
            config.window_height = Some(rect.3);
        }
    }
    let config = lock(settings).clone();
    lock(book).align(config.destination_folder().as_deref(), &config.prefix);
    persist(book, settings);
    let mut tick = tick.borrow_mut();
    tick.save_dirty = false;
    tick.index_dirty = false;
}

fn persist(book: &Arc<Mutex<SequenceBook>>, settings: &Arc<Mutex<Config>>) {
    let (id, manual) = {
        let book = lock(book);
        (book.next_id(), book.is_manual())
    };
    let mut config = lock(settings);
    config.next_id = id;
    config.manual_next = manual;
    config.save();
}

fn apply_config(ui: &AutoRenamer, config: &Config, id: u32) {
    with_apply(|| {
        ui.set_folder_path(SharedString::from(folder_text(config)));
        ui.set_base_name(SharedString::from(config.prefix.clone()));
        ui.set_id_length(config.id_width as i32);
        ui.set_copy_to_output_enabled(config.keep_original);
        ui.set_output_folder_path(SharedString::from(output_display(config)));
        ui.set_convert_enabled(config.convert);
        ui.set_output_format(SharedString::from(format_label(&config.format)));
        ui.set_jpg_quality(config.quality as f32);
        ui.set_resize_enabled(config.resize);
        ui.set_image_width(config.resize_width as i32);
        ui.set_image_height(config.resize_height as i32);
        ui.set_aspect_mode(if config.resize_stretch { 1 } else { 0 });
        ui.set_start_with_windows(config.start_with_windows);
        ui.set_activity_log(activity_model(&[]));
        ui.set_processed_count(0);
    });
    refresh_labels(ui, config, id);
}

fn refresh_labels(ui: &AutoRenamer, config: &Config, id: u32) {
    with_apply(|| {
        let next_id = id as i32;
        if ui.get_start_number() != next_id {
            ui.set_start_number(next_id);
        }
        let example = SharedString::from(example_name(config, id));
        if ui.get_next_filename() != example {
            ui.set_next_filename(example);
        }
        let running = is_ready(config);
        if ui.get_monitoring() != running {
            ui.set_monitoring(running);
        }
        let output = SharedString::from(output_display(config));
        if ui.get_output_folder_path() != output {
            ui.set_output_folder_path(output);
        }
        let width = config.id_width as i32;
        if ui.get_id_length() != width {
            ui.set_id_length(width);
        }
    });
}

fn restore_window(ui: &AutoRenamer, config: &Config) -> Option<(i32, i32)> {
    let (Some(x), Some(y)) = (config.window_x, config.window_y) else {
        return None;
    };
    let size = ui.window().size();
    if !placement_visible(x, y, size.width as i32, size.height as i32) {
        return None;
    }
    ui.window().set_position(PhysicalPosition::new(x, y));
    Some((x, y))
}

fn window_rect(ui: &AutoRenamer) -> (i32, i32, u32, u32) {
    let pos = ui.window().position();
    let size = ui.window().size();
    (pos.x, pos.y, size.width, size.height)
}

fn native_hwnd(ui: &AutoRenamer) -> Option<HWND> {
    let handle = ui.window().window_handle();
    let raw = handle.window_handle().ok()?.as_raw();
    match raw {
        RawWindowHandle::Win32(window) => Some(HWND(window.hwnd.get() as *mut std::ffi::c_void)),
        _ => None,
    }
}

fn drain_events(
    ui: &AutoRenamer,
    events: &Rc<RefCell<Receiver<UiEvent>>>,
    lines: &Rc<RefCell<Vec<LogLine>>>,
    tick: &Rc<RefCell<Tick>>,
) -> Option<(String, String)> {
    let mut offer = None;
    loop {
        let Ok(event) = events.borrow().try_recv() else {
            break;
        };
        match event {
            UiEvent::Status(_) => {}
            UiEvent::Log(text) => {
                let _ = text;
            }
            UiEvent::Renamed { from, to } => push_renamed(ui, lines, &from, &to),
            UiEvent::UpdateAvailable { version, url } => offer = Some((version, url)),
            UiEvent::UpdateReady => REQUEST_QUIT.store(true, Ordering::SeqCst),
            UiEvent::UpdateFailed(text) => {
                let _ = text;
                tick.borrow_mut().update_busy = false;
                ui.set_update_label(SharedString::from("Update failed"));
            }
        }
    }
    offer
}

fn offer_update(
    ui: &AutoRenamer,
    tick: &Rc<RefCell<Tick>>,
    hwnd: &Cell<isize>,
    ui_tx: &mpsc::Sender<UiEvent>,
    version: String,
    url: String,
) {
    {
        let mut tick = tick.borrow_mut();
        tick.update_version = version.clone();
        tick.update_url = url.clone();
    }
    ui.set_update_label(SharedString::from("Update available"));
    if tick.borrow().update_prompted || tick.borrow().update_busy {
        return;
    }
    tick.borrow_mut().update_prompted = true;
    begin_update(ui, tick, hwnd, ui_tx, version, url);
}

fn begin_update(
    ui: &AutoRenamer,
    tick: &Rc<RefCell<Tick>>,
    hwnd: &Cell<isize>,
    ui_tx: &mpsc::Sender<UiEvent>,
    version: String,
    url: String,
) {
    if tick.borrow().update_busy || url.is_empty() {
        return;
    }
    let raw = hwnd.get();
    let owner = HWND(raw as *mut std::ffi::c_void);
    if raw != 0 {
        show_window(owner);
    }
    let question = format!("Version {version} is available.\n\nUpdate now? Auto Renamer will close and reopen.");
    if !ask_yes_no(owner, &question) {
        return;
    }
    tick.borrow_mut().update_busy = true;
    ui.set_update_label(SharedString::from("Downloading…"));
    let tx = ui_tx.clone();
    std::thread::spawn(move || match update::download(&url) {
        Ok(path) => match update::install(&path) {
            Ok(()) => {
                let _ = tx.send(UiEvent::UpdateReady);
            }
            Err(err) => {
                let _ = tx.send(UiEvent::UpdateFailed(err));
            }
        },
        Err(err) => {
            let _ = tx.send(UiEvent::UpdateFailed(format!("Update failed: {err}")));
        }
    });
}

fn push_renamed(ui: &AutoRenamer, lines: &Rc<RefCell<Vec<LogLine>>>, from: &str, to: &str) {
    push_entry(ui, lines, from, to, true);
}

fn push_entry(
    ui: &AutoRenamer,
    lines: &Rc<RefCell<Vec<LogLine>>>,
    original: &str,
    renamed: &str,
    counts: bool,
) {
    let mut lines = lines.borrow_mut();
    lines.push(LogLine {
        at: Instant::now(),
        original: original.to_string(),
        renamed: renamed.to_string(),
        time: clock_now(),
        counts,
    });
    prune_lines(&mut lines);
    publish_log(ui, &lines);
}

fn prune_log(ui: &AutoRenamer, lines: &Rc<RefCell<Vec<LogLine>>>) {
    let mut lines = lines.borrow_mut();
    let before = lines.len();
    prune_lines(&mut lines);
    if lines.len() != before {
        publish_log(ui, &lines);
    }
}

fn prune_lines(lines: &mut Vec<LogLine>) {
    let cutoff = Instant::now() - LOG_KEEP;
    lines.retain(|line| line.at >= cutoff);
    if lines.len() > LOG_MAX {
        let extra = lines.len() - LOG_MAX;
        lines.drain(0..extra);
    }
}

fn publish_log(ui: &AutoRenamer, lines: &[LogLine]) {
    let count = lines.iter().filter(|line| line.counts).count() as i32;
    ui.set_activity_log(activity_model(lines));
    ui.set_processed_count(count);
}

fn activity_model(lines: &[LogLine]) -> slint::ModelRc<ActivityEntry> {
    let entries: Vec<ActivityEntry> = lines
        .iter()
        .map(|line| ActivityEntry {
            original: line.original.as_str().into(),
            renamed: line.renamed.as_str().into(),
            time: line.time.as_str().into(),
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(entries))
}

fn clock_now() -> String {
    let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}:{:02}", time.wHour, time.wMinute, time.wSecond)
}

fn announce_ffmpeg(ui: &mpsc::Sender<UiEvent>) {
    if convert::ffmpeg_status().is_some() {
        return;
    }
    let already = ffmpeg_sidecar::command::ffmpeg_is_installed();
    if !already {
        let _ = ui.send(UiEvent::Status("FFmpeg was not found. Downloading it…".into()));
        let _ = ui.send(UiEvent::Log("FFmpeg was not found. Downloading it…".into()));
    }
    match ensure_ffmpeg() {
        Ok(()) => {
            let _ = ui.send(UiEvent::Log(
                if already {
                    "FFmpeg was found on the system.".into()
                } else {
                    "FFmpeg is ready.".into()
                },
            ));
            if !already {
                let _ = ui.send(UiEvent::Status("FFmpeg is ready.".into()));
            }
        }
        Err(err) => {
            let _ = ui.send(UiEvent::Log(format!(
                "Could not download FFmpeg: {err}. Images are renamed without conversion."
            )));
        }
    }
}

fn example_name(config: &Config, id: u32) -> String {
    let Some(prefix) = clean_prefix(&config.prefix) else {
        if config.prefix.trim().is_empty() {
            return String::new();
        }
        return "invalid filename".into();
    };
    let width = config.id_width as usize;
    if config.convert {
        sequence_name(
            &prefix,
            id,
            width,
            OutputFormat::parse(&config.format).as_str(),
        )
    } else {
        format!("{prefix}_{id:0width$}.*")
    }
}

fn folder_text(config: &Config) -> String {
    config
        .folder
        .as_ref()
        .map(|folder| folder.display().to_string())
        .unwrap_or_default()
}

fn output_display(config: &Config) -> String {
    let raw = config.output_folder.trim();
    if raw.is_empty() {
        return String::new();
    }
    if std::path::Path::new(raw).is_absolute() {
        return raw.to_string();
    }
    if let Some(watch) = config.folder.as_deref() {
        return config::output_folder_path(watch, raw).display().to_string();
    }
    if raw.eq_ignore_ascii_case("renamed") {
        return String::new();
    }
    raw.to_string()
}

fn remember_output(config: &mut Config, shown: &str) {
    let shown = shown.trim();
    if shown.is_empty() {
        if config.output_folder.trim().is_empty() {
            config.output_folder = "renamed".into();
        }
        return;
    }
    if let Some(watch) = config.folder.clone() {
        let resolved = config::output_folder_path(&watch, &config.output_folder);
        if shown.eq_ignore_ascii_case(&resolved.display().to_string()) || shown == config.output_folder
        {
            return;
        }
    } else if shown == config.output_folder {
        return;
    }
    config.output_folder = shown.to_string();
}

fn format_label(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "JPG",
        "webp" => "WEBP",
        "bmp" => "BMP",
        "avif" => "AVIF",
        _ => "PNG",
    }
}

fn format_code(label: &str) -> &'static str {
    match label.to_ascii_uppercase().as_str() {
        "JPG" | "JPEG" => "jpg",
        "WEBP" => "webp",
        "BMP" => "bmp",
        "AVIF" => "avif",
        _ => "png",
    }
}

fn is_ready(config: &Config) -> bool {
    config.folder.as_ref().is_some_and(|folder| folder.is_dir())
        && clean_prefix(&config.prefix).is_some()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}
