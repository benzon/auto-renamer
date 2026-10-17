use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE,
    HWND, LPARAM, LRESULT, WIN32_ERROR, WPARAM,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, MonitorFromRect, ReleaseDC, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, MONITOR_DEFAULTTONULL,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, MoveFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::FileOpenDialog;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IShellItem, SHCreateItemFromParsingName, SetWindowSubclass, ShellExecuteW,
    Shell_NotifyIconW, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, NIF_ICON,
    NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICON_VERSION_4,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyMenu, FindWindowW,
    GetAncestor, GetCursorPos, GetWindowLongW, MessageBoxW, PostMessageW, SendMessageW,
    SetForegroundWindow, ShowWindow,
    TrackPopupMenu, HICON, ICONINFO, ICON_BIG, ICON_SMALL, IDYES, MB_ICONINFORMATION,
    MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_YESNO, MF_STRING,
    SC_MAXIMIZE, SC_MINIMIZE, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL, SWP_FRAMECHANGED,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongW, SetWindowPos, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_CLOSE, WM_CONTEXTMENU, GA_ROOT, GWL_STYLE, WS_MAXIMIZEBOX,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETICON, WM_SYSCOMMAND,
};

pub const WM_TRAY: u32 = 0x8000 + 17;
pub static REQUEST_QUIT: AtomicBool = AtomicBool::new(false);
pub static TRAY_READY: AtomicBool = AtomicBool::new(false);

const GENERIC_READ: u32 = 0x8000_0000;
const FILE_LIST_DIRECTORY: u32 = 0x0001;

pub struct ComGuard(bool);

impl ComGuard {
    pub fn init() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Self(hr.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub enum InstanceLock {
    Primary(HANDLE),
    AlreadyRunning,
}

pub fn claim_single_instance() -> InstanceLock {
    unsafe {
        SetLastError(WIN32_ERROR(0));
        match CreateMutexW(None, false, w!("Local\\AutoRenamer.SingleInstance")) {
            Ok(handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    reveal_running_window();
                    let _ = CloseHandle(handle);
                    InstanceLock::AlreadyRunning
                } else {
                    InstanceLock::Primary(handle)
                }
            }
            Err(_) => InstanceLock::Primary(HANDLE::default()),
        }
    }
}

pub fn set_start_with_windows(enabled: bool) -> Result<(), String> {
    let command = if enabled {
        let exe = std::env::current_exe().map_err(|err| err.to_string())?;
        Some(wide_text(&format!("\"{}\"", exe.display())))
    } else {
        None
    };
    let subkey = wide_text(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let value_name = w!("Auto Renamer");
    unsafe {
        let mut key = windows::Win32::System::Registry::HKEY::default();
        let created = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if created.is_err() {
            return Err(created.to_hresult().message().to_string());
        }
        let written = if let Some(command) = command.as_ref() {
            let bytes = std::slice::from_raw_parts(command.as_ptr() as *const u8, command.len() * 2);
            RegSetValueExW(key, value_name, Some(0), REG_SZ, Some(bytes))
        } else {
            RegDeleteValueW(key, value_name)
        };
        let _ = RegCloseKey(key);
        if written == ERROR_FILE_NOT_FOUND && !enabled {
            return Ok(());
        }
        if written.is_err() {
            return Err(written.to_hresult().message().to_string());
        }
        Ok(())
    }
}

fn reveal_running_window() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(None, w!("Auto Renamer")) {
            if !hwnd.0.is_null() {
                show_window(hwnd);
                return;
            }
        }
        let _ = MessageBoxW(
            None,
            w!("Auto Renamer is already running. Open it from the system tray."),
            w!("Auto Renamer"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn wide_text(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(path: &Path) -> Vec<u16> {
    let mut text = path.as_os_str().to_string_lossy().replace('/', "\\");
    if text.len() >= 240 && !text.starts_with("\\\\?\\") {
        if let Some(rest) = text.strip_prefix("\\\\") {
            text = format!("\\\\?\\UNC\\{rest}");
        } else {
            text = format!("\\\\?\\{text}");
        }
    }
    std::ffi::OsStr::new(&text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn exclusively_readable(path: &Path) -> bool {
    let wide = wide_path(path);
    unsafe {
        match CreateFileW(
            PCWSTR::from_raw(wide.as_ptr()),
            GENERIC_READ,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

pub fn open_directory(path: &Path) -> windows::core::Result<HANDLE> {
    let wide = wide_path(path);
    unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide.as_ptr()),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
}

pub fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    let from_w = wide_path(from);
    let to_w = wide_path(to);
    unsafe {
        MoveFileW(
            PCWSTR::from_raw(from_w.as_ptr()),
            PCWSTR::from_raw(to_w.as_ptr()),
        )
        .map_err(|err| err.to_string())
    }
}

pub fn delete_file(path: &Path) -> Result<(), String> {
    let wide = wide_path(path);
    unsafe { DeleteFileW(PCWSTR::from_raw(wide.as_ptr())).map_err(|err| err.to_string()) }
}

pub fn strip_maximize(hwnd: HWND) {
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let style = style & !WS_MAXIMIZEBOX.0;
        SetWindowLongW(hwnd, GWL_STYLE, style as i32);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

pub fn centered_on_primary(width: i32, height: i32) -> (i32, i32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            let x = work.left + (work.right - work.left - width).max(0) / 2;
            let y = work.top + (work.bottom - work.top - height).max(0) / 2;
            return (x, y);
        }
    }
    (80, 80)
}

pub fn fit_on_work_area(x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let rect = RECT {
        left: x,
        top: y,
        right: x.saturating_add(width),
        bottom: y.saturating_add(height),
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    unsafe {
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            let max_w = (work.right - work.left).max(640);
            let max_h = (work.bottom - work.top).max(480);
            let width = width.clamp(640, max_w);
            let height = height.clamp(480, max_h);
            let x = x.clamp(work.left, work.right.saturating_sub(width).max(work.left));
            let y = y.clamp(work.top, work.bottom.saturating_sub(height).max(work.top));
            return (x, y, width, height);
        }
    }
    (x, y, width, height)
}

pub fn placement_visible(x: i32, y: i32, width: i32, height: i32) -> bool {
    if width < 200 || height < 200 {
        return false;
    }
    let rect = RECT {
        left: x,
        top: y,
        right: x.saturating_add(width),
        bottom: y.saturating_add(height),
    };
    unsafe { !MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_invalid() }
}

pub fn pick_folder(owner: HWND, current: Option<&Path>, title: &str) -> Option<PathBuf> {
    unsafe {
        let dialog: windows::Win32::UI::Shell::IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let mut options = dialog.GetOptions().ok()?;
        options |= FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
        dialog.SetOptions(options).ok()?;
        let title_wide = wide_text(title);
        dialog.SetTitle(PCWSTR::from_raw(title_wide.as_ptr())).ok()?;
        dialog.SetOkButtonLabel(w!("Select folder")).ok()?;
        if let Some(folder) = current.filter(|path| path.is_dir()) {
            let wide = wide_path(folder);
            let bind: Option<&windows::Win32::System::Com::IBindCtx> = None;
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                PCWSTR::from_raw(wide.as_ptr()),
                bind,
            ) {
                let _ = dialog.SetFolder(&item);
            }
        }
        if dialog.Show(Some(owner)).is_err() {
            return None;
        }
        let item = dialog.GetResult().ok()?;
        let name: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        path.map(PathBuf::from)
    }
}

pub fn open_url(url: &str) {
    let wide = wide_text(url);
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

pub fn ask_yes_no(owner: HWND, text: &str) -> bool {
    let wide = wide_text(text);
    let owner = if owner.0.is_null() { None } else { Some(owner) };
    unsafe {
        MessageBoxW(
            owner,
            PCWSTR(wide.as_ptr()),
            w!("Auto Renamer"),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

pub fn open_folder(path: &Path) {
    if !path.is_dir() {
        return;
    }
    let wide = wide_path(path);
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("explore"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

pub struct TrayIcon {
    hwnd: HWND,
    icon: HICON,
    added: bool,
}

impl TrayIcon {
    pub fn install(owner: HWND) -> Self {
        let owner = root_window(owner);
        unsafe {
            let _ = SetWindowSubclass(owner, Some(subclass_proc), 1, 0);
        }
        let icon = raster_icon(&app_icon_rgba(32), 32);
        if !icon.0.is_null() {
            unsafe {
                let _ = SendMessageW(
                    owner,
                    WM_SETICON,
                    Some(WPARAM(ICON_BIG as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
                let _ = SendMessageW(
                    owner,
                    WM_SETICON,
                    Some(WPARAM(ICON_SMALL as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
            }
        }
        let mut tray = Self {
            hwnd: owner,
            icon,
            added: false,
        };
        tray.ensure_added();
        tray
    }

    pub fn is_added(&self) -> bool {
        self.added
    }

    pub fn ensure_added(&mut self) {
        if self.added || self.icon.0.is_null() || self.hwnd.0.is_null() {
            TRAY_READY.store(self.added, Ordering::SeqCst);
            return;
        }
        let mut data = notify_data(self.hwnd, self.icon);
        unsafe {
            let added = if Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                true
            } else {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                Shell_NotifyIconW(NIM_ADD, &data).as_bool()
            };
            if added {
                data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
            self.added = added;
            TRAY_READY.store(added, Ordering::SeqCst);
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        TRAY_READY.store(false, Ordering::SeqCst);
        if self.added && !self.icon.0.is_null() {
            let data = notify_data(self.hwnd, self.icon);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            }
        }
    }
}

fn root_window(hwnd: HWND) -> HWND {
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.0.is_null() { hwnd } else { root }
}

fn notify_data(hwnd: HWND, icon: HICON) -> windows::Win32::UI::Shell::NOTIFYICONDATAW {
    let mut data: windows::Win32::UI::Shell::NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of_val(&data) as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon;
    let tip: Vec<u16> = "Auto Renamer"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let count = tip.len().min(data.szTip.len());
    data.szTip[..count].copy_from_slice(&tip[..count]);
    data
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    if msg == WM_TRAY {
        on_tray(hwnd, lparam);
        return LRESULT(0);
    }
    if msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) == SC_MAXIMIZE as usize {
        return LRESULT(0);
    }
    if TRAY_READY.load(Ordering::SeqCst)
        && msg == WM_SYSCOMMAND
        && (wparam.0 & 0xFFF0) == SC_MINIMIZE as usize
    {
        let _ = ShowWindow(hwnd, SW_HIDE);
        return LRESULT(0);
    }
    if TRAY_READY.load(Ordering::SeqCst) && msg == WM_CLOSE {
        let _ = ShowWindow(hwnd, SW_HIDE);
        return LRESULT(0);
    }
    let _ = (id, data);
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn on_tray(hwnd: HWND, lparam: LPARAM) {
    let mouse = (lparam.0 as u32) & 0xFFFF;
    match mouse {
        WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_window(hwnd),
        WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
        _ => {}
    }
}

pub fn hide_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

pub fn show_window(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("Show Auto Renamer"));
        let _ = AppendMenuW(menu, MF_STRING, 2, w!("Quit"));
        let mut point = windows::Win32::Foundation::POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            point.x,
            point.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 {
            1 => show_window(hwnd),
            2 => REQUEST_QUIT.store(true, Ordering::SeqCst),
            _ => {}
        }
    }
}

fn app_icon_rgba(size: usize) -> Vec<u8> {
    match decode_app_icon() {
        Some((pixels, width, height)) => scale_rgba(&pixels, width, height, size),
        None => icon_rgba(size),
    }
}

fn decode_app_icon() -> Option<(Vec<u8>, usize, usize)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!(
        "../assets/icon.png"
    )));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).ok()?;
    let bytes = &buffer[..info.buffer_size()];
    let width = info.width as usize;
    let height = info.height as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => {
            let mut expanded = Vec::with_capacity(width * height * 4);
            for pixel in bytes.chunks(3) {
                expanded.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            expanded
        }
        _ => return None,
    };
    if rgba.len() != width * height * 4 {
        return None;
    }
    Some((rgba, width, height))
}

fn scale_rgba(source: &[u8], width: usize, height: usize, size: usize) -> Vec<u8> {
    let mut scaled = vec![0u8; size * size * 4];
    for y in 0..size {
        let y0 = y * height / size;
        let y1 = ((y + 1) * height / size).max(y0 + 1);
        for x in 0..size {
            let x0 = x * width / size;
            let x1 = ((x + 1) * width / size).max(x0 + 1);
            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let index = (sy * width + sx) * 4;
                    for channel in 0..4 {
                        sum[channel] += source[index + channel] as u32;
                    }
                    count += 1;
                }
            }
            let index = (y * size + x) * 4;
            for channel in 0..4 {
                scaled[index + channel] = (sum[channel] / count) as u8;
            }
        }
    }
    scaled
}

pub fn icon_rgba(size: usize) -> Vec<u8> {
    let mut pixels = vec![0u8; size * size * 4];
    let blue = [37, 99, 235, 255];
    let white = [255, 255, 255, 255];
    for y in 0..size {
        for x in 0..size {
            let i = (y * size + x) * 4;
            pixels[i..i + 4].copy_from_slice(&blue);
        }
    }
    let s = size as i32;
    for y in (s * 8 / 32)..(s * 24 / 32) {
        for x in (s * 6 / 32)..(s * 15 / 32) {
            let edge = x == s * 6 / 32 || x == s * 14 / 32 || y == s * 8 / 32 || y == s * 23 / 32;
            if edge {
                stamp(&mut pixels, size, x, y, white);
            }
        }
    }
    for t in 0..(s * 8 / 32) {
        let x = s * 18 / 32 + t;
        for y in (s * 15 / 32 - t)..=(s * 16 / 32 + t) {
            stamp(&mut pixels, size, x, y, white);
        }
    }
    for y in (s * 14 / 32)..(s * 18 / 32) {
        for x in (s * 14 / 32)..(s * 22 / 32) {
            stamp(&mut pixels, size, x, y, white);
        }
    }
    pixels
}

fn stamp(pixels: &mut [u8], size: usize, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
        return;
    }
    let i = (y as usize * size + x as usize) * 4;
    pixels[i..i + 4].copy_from_slice(&color);
}

fn raster_icon(rgba: &[u8], size: i32) -> HICON {
    unsafe {
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            bmiColors: [Default::default()],
        };
        let screen: HDC = GetDC(None);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color: HBITMAP =
            match CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bitmap) => bitmap,
                Err(_) => {
                    let _ = ReleaseDC(None, screen);
                    return HICON::default();
                }
            };
        let mut bgra = Vec::with_capacity(rgba.len());
        for px in rgba.chunks(4) {
            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        if !bits.is_null() {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
        }
        let mask_bits = vec![0u8; (size * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
        let icon = if mask.0.is_null() {
            HICON::default()
        } else {
            let info = ICONINFO {
                fIcon: true.into(),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: color,
            };
            let icon = CreateIconIndirect(&info).unwrap_or_default();
            let _ = DeleteObject(mask.into());
            icon
        };
        let _ = DeleteObject(color.into());
        let _ = ReleaseDC(None, screen);
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "auto-renamer-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn exclusive_open_sees_a_lock_and_a_finished_file() {
        let dir = scratch("lock");
        let path = dir.join("frame.png");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"png").unwrap();
        let locked = fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(!exclusively_readable(&path));
        drop(locked);
        drop(file);
        assert!(exclusively_readable(&path));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_refuses_to_replace_an_existing_render() {
        let dir = scratch("move");
        let from = dir.join("new.png");
        let to = dir.join("screenshot_00001.png");
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        assert!(move_file(&from, &to).is_err());
        assert_eq!(fs::read(&from).unwrap(), b"new");
        assert_eq!(fs::read(&to).unwrap(), b"old");
        let free = dir.join("screenshot_00002.png");
        assert!(move_file(&from, &free).is_ok());
        assert!(!from.exists());
        assert_eq!(fs::read(&free).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }
}
