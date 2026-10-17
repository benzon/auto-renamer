//! Opens `AutoRenamer` with the demo data in `preview.slint`. No watcher, no config.

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    AutoRenamer::new()?.run()
}
