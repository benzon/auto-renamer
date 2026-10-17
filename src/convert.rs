use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use ffmpeg_sidecar::command::FfmpegCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Png,
    Jpg,
    Webp,
    Avif,
    Bmp,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Self::Jpg,
            "webp" => Self::Webp,
            "avif" => Self::Avif,
            "bmp" => Self::Bmp,
            _ => Self::Png,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpg => "jpg",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Bmp => "bmp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpg => "JPG",
            Self::Webp => "WebP",
            Self::Avif => "AVIF",
            Self::Bmp => "BMP",
        }
    }
}

pub fn same_format(source_ext: &str, target: OutputFormat) -> bool {
    OutputFormat::from_extension(source_ext) == Some(target)
}

impl OutputFormat {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpg),
            "webp" => Some(Self::Webp),
            "avif" => Some(Self::Avif),
            "bmp" => Some(Self::Bmp),
            _ => None,
        }
    }
}

static ENSURED: Mutex<Option<Result<(), String>>> = Mutex::new(None);
static ENSURING: AtomicBool = AtomicBool::new(false);

pub fn ffmpeg_status() -> Option<Result<(), String>> {
    ENSURED.lock().ok().and_then(|guard| guard.clone())
}

pub fn ensure_ffmpeg() -> Result<(), String> {
    if let Some(done) = ffmpeg_status() {
        return done;
    }
    if ENSURING.swap(true, Ordering::SeqCst) {
        while ENSURING.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        return ffmpeg_status().unwrap_or_else(|| Err("FFmpeg was not prepared.".into()));
    }
    let result = download_if_missing();
    if let Ok(mut slot) = ENSURED.lock() {
        *slot = Some(result.clone());
    }
    ENSURING.store(false, Ordering::SeqCst);
    result
}

fn download_if_missing() -> Result<(), String> {
    if ffmpeg_sidecar::command::ffmpeg_is_installed() {
        return Ok(());
    }
    std::env::set_var("KEEP_ONLY_FFMPEG", "1");
    ffmpeg_sidecar::download::auto_download().map_err(|err| err.to_string())?;
    if ffmpeg_sidecar::command::ffmpeg_is_installed() {
        Ok(())
    } else {
        Err("FFmpeg was downloaded, but it cannot be started.".into())
    }
}

pub fn convert_image(
    source: &Path,
    destination: &Path,
    format: OutputFormat,
    quality: u8,
    max_width: u32,
    max_height: u32,
    stretch: bool,
) -> Result<(), String> {
    ensure_ffmpeg()?;
    let quality = quality.clamp(1, 100);
    let first = convert_once(
        source, destination, format, quality, max_width, max_height, stretch, false,
    );
    if first.is_ok() || format != OutputFormat::Avif {
        return first;
    }
    let _ = std::fs::remove_file(destination);
    convert_once(
        source, destination, format, quality, max_width, max_height, stretch, true,
    )
}

pub fn within_limits(
    size: Option<(u32, u32)>,
    max_width: u32,
    max_height: u32,
    stretch: bool,
) -> bool {
    if max_width == 0 && max_height == 0 {
        return true;
    }
    let Some((width, height)) = size else {
        return false;
    };
    if stretch && max_width > 0 && max_height > 0 {
        return width == max_width && height == max_height;
    }
    (max_width == 0 || width <= max_width) && (max_height == 0 || height <= max_height)
}

pub fn image_size(path: &Path) -> Option<(u32, u32)> {
    ensure_ffmpeg().ok()?;
    let mut command = FfmpegCommand::new();
    command
        .hide_banner()
        .args(["-i", &path.to_string_lossy()]);
    let mut child = command.spawn().ok()?;
    let stderr = child.as_inner_mut().stderr.take();
    let stdout = child.as_inner_mut().stdout.take();
    drop(child.as_inner_mut().stdin.take());
    let err_thread = std::thread::spawn(move || read_pipe(stderr));
    let out_thread = std::thread::spawn(move || read_pipe(stdout));
    let _ = child.wait();
    let text = err_thread.join().unwrap_or_default();
    let _ = out_thread.join();
    parse_video_size(&text)
}

pub fn video_filter(
    format: OutputFormat,
    max_width: u32,
    max_height: u32,
    stretch: bool,
) -> Option<String> {
    let even = matches!(format, OutputFormat::Jpg | OutputFormat::Avif);
    let fit = match (max_width, max_height, stretch) {
        (0, 0, _) => None,
        (width, height, true) if width > 0 && height > 0 => {
            Some(format!("scale={width}:{height}:flags=lanczos"))
        }
        (width, 0, _) => Some(format!("scale='min(iw,{width})':-2:flags=lanczos")),
        (0, height, _) => Some(format!("scale=-2:'min(ih,{height})':flags=lanczos")),
        (width, height, _) => Some(format!(
            "scale='min(iw,{width})':'min(ih,{height})':force_original_aspect_ratio=decrease:flags=lanczos"
        )),
    };
    match (fit, even) {
        (Some(fit), true) => {
            Some(format!("{fit},scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p"))
        }
        (Some(fit), false) => Some(fit),
        (None, true) => {
            Some("scale=trunc(iw/2)*2:trunc(ih/2)*2:flags=lanczos,format=yuv420p".into())
        }
        (None, false) => None,
    }
}

fn parse_video_size(text: &str) -> Option<(u32, u32)> {
    let line = text.lines().find(|line| line.contains("Video:"))?;
    for token in line.split(|c: char| c.is_whitespace() || c == ',') {
        let Some((width, height)) = token.split_once('x') else {
            continue;
        };
        let Ok(width) = width.parse::<u32>() else {
            continue;
        };
        let Ok(height) = height.parse::<u32>() else {
            continue;
        };
        if width > 0 && height > 0 && width < 100_000 && height < 100_000 {
            return Some((width, height));
        }
    }
    None
}

fn convert_once(
    source: &Path,
    destination: &Path,
    format: OutputFormat,
    quality: u8,
    max_width: u32,
    max_height: u32,
    stretch: bool,
    avif_fallback: bool,
) -> Result<(), String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(filter) = video_filter(format, max_width, max_height, stretch) {
        args.extend(["-vf".into(), filter]);
    }
    match format {
        OutputFormat::Png => {
            args.extend([
                "-c:v".into(),
                "png".into(),
                "-compression_level".into(),
                "9".into(),
            ]);
        }
        OutputFormat::Jpg => {
            let qscale = 31 - ((quality - 1) as u32 * 29 / 99);
            args.extend(["-c:v".into(), "mjpeg".into(), "-q:v".into(), qscale.to_string()]);
        }
        OutputFormat::Webp => {
            args.extend([
                "-c:v".into(),
                "libwebp".into(),
                "-quality".into(),
                quality.to_string(),
            ]);
        }
        OutputFormat::Avif if !avif_fallback => {
            let crf = 63 - (quality as u32 * 45 / 100);
            args.extend([
                "-c:v".into(),
                "libsvtav1".into(),
                "-preset".into(),
                "8".into(),
                "-crf".into(),
                crf.to_string(),
            ]);
        }
        OutputFormat::Avif => {
            let crf = 63 - (quality as u32 * 50 / 100);
            args.extend([
                "-c:v".into(),
                "libaom-av1".into(),
                "-still-picture".into(),
                "1".into(),
                "-cpu-used".into(),
                "6".into(),
                "-crf".into(),
                crf.to_string(),
            ]);
        }
        OutputFormat::Bmp => {
            args.extend(["-c:v".into(), "bmp".into()]);
        }
    }
    args.extend(["-frames:v".into(), "1".into()]);
    let container = match format {
        OutputFormat::Jpg | OutputFormat::Png | OutputFormat::Bmp => "image2",
        OutputFormat::Webp => "webp",
        OutputFormat::Avif => "avif",
    };

    let mut command = FfmpegCommand::new();
    command
        .hide_banner()
        .overwrite()
        .input(source)
        .args(args.iter().map(String::as_str))
        .format(container)
        .output(destination);

    let mut child = command
        .spawn()
        .map_err(|err| format!("Could not start ffmpeg: {err}"))?;
    let stderr = child.as_inner_mut().stderr.take();
    let stdout = child.as_inner_mut().stdout.take();
    drop(child.as_inner_mut().stdin.take());
    let err_thread = std::thread::spawn(move || read_pipe(stderr));
    let out_thread = std::thread::spawn(move || read_pipe(stdout));
    let status = child.wait().map_err(|err| err.to_string())?;
    let err_text = err_thread.join().unwrap_or_default();
    let _ = out_thread.join();
    if status.success() && destination.is_file() {
        Ok(())
    } else {
        let detail = err_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .last()
            .unwrap_or("ffmpeg failed without an error message");
        Err(detail.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_filter_fits_inside_the_box_and_does_not_enlarge() {
        let filter = video_filter(OutputFormat::Png, 1920, 1080, false).unwrap();
        assert!(filter.contains("min(iw,1920)"));
        assert!(filter.contains("min(ih,1080)"));
        assert!(filter.contains("force_original_aspect_ratio=decrease"));
        assert!(video_filter(OutputFormat::Png, 0, 0, false).is_none());
        let jpg = video_filter(OutputFormat::Jpg, 0, 0, false).unwrap();
        assert!(jpg.contains("yuv420p"));
        let stretch = video_filter(OutputFormat::Png, 800, 600, true).unwrap();
        assert!(stretch.contains("scale=800:600"));
        assert!(!stretch.contains("force_original_aspect_ratio"));
    }

    #[test]
    fn parses_the_video_size_from_ffmpeg() {
        let text = "Stream #0:0: Video: png, rgba(pc), 1920x1080 [SAR 1:1 DAR 16:9]\n";
        assert_eq!(parse_video_size(text), Some((1920, 1080)));
        assert!(within_limits(Some((1280, 720)), 1920, 1080, false));
        assert!(!within_limits(Some((3840, 2160)), 1920, 1080, false));
        assert!(!within_limits(None, 1920, 1080, false));
        assert!(!within_limits(Some((1280, 720)), 1920, 1080, true));
        assert!(within_limits(Some((1920, 1080)), 1920, 1080, true));
    }
}

fn read_pipe(pipe: Option<impl std::io::Read>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut text = String::new();
    let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
    text
}
