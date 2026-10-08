//! Types and helpers shared by the hive daemon, TUI and CLI.

pub mod agents;
pub mod codec;
pub mod config;
pub mod paths;
pub mod protocol;
pub mod sanitize;
pub mod template;

/// Bumped whenever `protocol` changes shape; the TUI refuses a mismatched daemon.
pub const PROTOCOL_VERSION: u32 = 1;

pub fn new_id() -> String {
    ulid::Ulid::new().to_string().to_lowercase()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
