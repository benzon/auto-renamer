#[derive(Debug)]
pub enum UiEvent {
    #[allow(dead_code)]
    Status(String),
    Log(String),
    Renamed { from: String, to: String },
    UpdateAvailable { version: String, url: String },
    UpdateReady,
    UpdateFailed(String),
}
