use std::path::{Path, PathBuf};

use serde::Deserialize;

const LATEST: &str = "https://api.github.com/repos/benzon/auto-renamer/releases/latest";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub struct Offer {
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub fn restart_after_arg() -> Option<u32> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--restart-after" {
            return args.next()?.parse().ok();
        }
    }
    None
}

pub fn wait_for_exit(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return;
        };
        let _ = WaitForSingleObject(handle, 20_000);
        let _ = CloseHandle(handle);
    }
}

pub fn remove_previous_exe() {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let previous = current.with_file_name("auto-renamer.exe.old");
    let _ = std::fs::remove_file(previous);
}

pub fn available() -> Option<Offer> {
    let body = http_get(LATEST, 1_000_000, 20_000).ok()?;
    let release: GithubRelease = serde_json::from_slice(&body).ok()?;
    if !is_newer(&release.tag_name, CURRENT) {
        return None;
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name.eq_ignore_ascii_case("auto-renamer.exe"))?;
    if !trusted_download_url(&asset.browser_download_url) {
        return None;
    }
    Some(Offer {
        version: display_version(&release.tag_name),
        url: asset.browser_download_url,
    })
}

pub fn download(url: &str) -> Result<PathBuf, String> {
    if !trusted_download_url(url) {
        return Err("The download address was rejected.".into());
    }
    let bytes = http_get(url, 80_000_000, 120_000)?;
    if bytes.len() < 500_000 || !bytes.starts_with(b"MZ") {
        return Err("The downloaded file was not a program.".into());
    }
    let dir = std::env::temp_dir().join("Auto Renamer");
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let partial = dir.join("update.exe.partial");
    let staged = dir.join("update.exe");
    std::fs::write(&partial, &bytes).map_err(|err| err.to_string())?;
    let _ = std::fs::remove_file(&staged);
    std::fs::rename(&partial, &staged).map_err(|err| err.to_string())?;
    Ok(staged)
}

pub fn install(staged: &Path) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|err| err.to_string())?;
    let backup = current.with_file_name("auto-renamer.exe.old");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&current, &backup).map_err(|err| format!("Could not replace the program: {err}"))?;
    if let Err(err) = std::fs::copy(staged, &current) {
        let _ = std::fs::rename(&backup, &current);
        return Err(format!("Could not replace the program: {err}"));
    }
    let _ = std::fs::remove_file(staged);
    std::process::Command::new(&current)
        .arg("--restart-after")
        .arg(std::process::id().to_string())
        .spawn()
        .map_err(|err| format!("The update is installed. Restart Auto Renamer to use it. {err}"))?;
    Ok(())
}

fn display_version(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch: String = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if patch.is_empty() {
        return None;
    }
    Some((major, minor, patch.parse().ok()?))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn trusted_download_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = host.split('@').next_back().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host == "github.com"
        || host.ends_with(".github.com")
        || host.ends_with(".githubusercontent.com")
}

fn http_get(url: &str, max_bytes: usize, receive_ms: i32) -> Result<Vec<u8>, String> {
    use std::ffi::c_void;

    use windows::core::{w, PCWSTR, PWSTR};
    use windows::Win32::Networking::WinHttp::{
        WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
        WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
        WinHttpSendRequest, WinHttpSetTimeouts, URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
        WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    };

    let wide: Vec<u16> = url.encode_utf16().collect();
    let mut host_buf = vec![0u16; 256];
    let mut path_buf = vec![0u16; 4096];
    let mut extra_buf = vec![0u16; 2048];
    let mut parts: URL_COMPONENTS = unsafe { std::mem::zeroed() };
    parts.dwStructSize = std::mem::size_of::<URL_COMPONENTS>() as u32;
    parts.lpszHostName = PWSTR(host_buf.as_mut_ptr());
    parts.dwHostNameLength = host_buf.len() as u32;
    parts.lpszUrlPath = PWSTR(path_buf.as_mut_ptr());
    parts.dwUrlPathLength = path_buf.len() as u32;
    parts.lpszExtraInfo = PWSTR(extra_buf.as_mut_ptr());
    parts.dwExtraInfoLength = extra_buf.len() as u32;

    unsafe {
        WinHttpCrackUrl(&wide, 0, &mut parts).map_err(|err| err.to_string())?;
    }
    if parts.nScheme.0 != 2 {
        return Err("Only https downloads are accepted.".into());
    }
    let host = String::from_utf16_lossy(&host_buf[..parts.dwHostNameLength as usize]);
    let path = String::from_utf16_lossy(&path_buf[..parts.dwUrlPathLength as usize]);
    let extra = String::from_utf16_lossy(&extra_buf[..parts.dwExtraInfoLength as usize]);
    let object = format!("{path}{extra}");
    let host_wide: Vec<u16> = host.encode_utf16().chain(std::iter::once(0)).collect();
    let object_wide: Vec<u16> = object.encode_utf16().chain(std::iter::once(0)).collect();
    let port = if parts.nPort == 0 { 443 } else { parts.nPort };

    unsafe {
        let session = Inet::new(WinHttpOpen(
            w!("Auto Renamer"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, 8_000, 8_000, 20_000, receive_ms).map_err(|err| err.to_string())?;
        let connect = Inet::new(WinHttpConnect(session.0, PCWSTR(host_wide.as_ptr()), port, 0))?;
        let request = Inet::new(WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            PCWSTR(object_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|err| err.to_string())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|err| err.to_string())?;
        let mut status = 0u32;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut status_size,
            std::ptr::null_mut(),
        )
        .map_err(|err| err.to_string())?;
        if status == 404 {
            return Err("No release was published.".into());
        }
        if status != 200 {
            return Err(format!("The server returned status {status}."));
        }
        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(|err| err.to_string())?;
            if available == 0 {
                break;
            }
            let mut chunk = vec![0u8; available as usize];
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut c_void,
                available,
                &mut read,
            )
            .map_err(|err| err.to_string())?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > max_bytes {
                return Err("The download was larger than expected.".into());
            }
        }
        Ok(body)
    }
}

struct Inet(*mut std::ffi::c_void);

impl Inet {
    fn new(handle: *mut std::ffi::c_void) -> Result<Self, String> {
        if handle.is_null() {
            Err(windows::core::Error::from_win32().message().to_string())
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for Inet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_release_is_newer() {
        assert!(is_newer("v1.1.0", "1.0.0"));
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
        assert!(!is_newer("latest", "1.0.0"));
    }

    #[test]
    fn download_urls_stay_on_github() {
        assert!(trusted_download_url(
            "https://github.com/benzon/auto-renamer/releases/download/v1.0.1/auto-renamer.exe"
        ));
        assert!(trusted_download_url(
            "https://objects.githubusercontent.com/github-production-release-asset/file"
        ));
        assert!(!trusted_download_url("http://github.com/benzon/auto-renamer.exe"));
        assert!(!trusted_download_url("https://example.com/auto-renamer.exe"));
    }
}
