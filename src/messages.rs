//! Worker and watcher events for the window.

#[derive(Debug)]
pub enum UiEvent {
    /// Worker still sends these. The window ignores them.
    #[allow(dead_code)]
    Status(String),
    Log(String),
    /// Successful rename or copy. Both fields are file names, not full paths.
    Renamed { from: String, to: String },
}
