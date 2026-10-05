/// Pure screen/transcript parsing boundary. Input text has already been acquired by an adapter;
/// the implementation must not read files, contact a host, or inspect application state.
pub trait CaptureParser: Send + Sync {
    fn still_busy(&self, screen: &str) -> bool;
    fn awaits_input(&self, screen: &str) -> bool;
    fn extract_reply(&self, screen: &str) -> Option<String>;
    fn noise_line(&self, line: &str) -> bool;
    fn activity(&self, screen: &str) -> Option<String>;
}
