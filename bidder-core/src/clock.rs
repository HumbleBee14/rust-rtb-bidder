use std::time::{SystemTime, UNIX_EPOCH};

pub fn current_hour_of_day() -> u8 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    ((secs % 86400) / 3600) as u8
}
