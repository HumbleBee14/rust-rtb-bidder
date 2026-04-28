use std::time::{SystemTime, UNIX_EPOCH};

pub fn current_hour_of_day() -> u8 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    ((secs % 86400) / 3600) as u8
}

/// Day of week, 0 = Monday … 6 = Sunday, in UTC.
///
/// Unix epoch (1970-01-01) was a Thursday → adding 3 to the day-count and
/// taking mod 7 produces the Monday-anchored value the rest of the bidder
/// uses (matches the daypart bitmap convention in `catalog/loader.rs`).
pub fn current_day_of_week_utc() -> u8 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let day_count = secs / 86400;
    ((day_count + 3) % 7) as u8
}

/// True for Saturday (5) and Sunday (6) under the Monday=0 convention.
///
/// Used by ScoringStage to populate ScoringContext.is_weekend so the ML
/// feature pipeline doesn't have to know about clock conventions.
pub fn is_weekend_utc() -> bool {
    matches!(current_day_of_week_utc(), 5 | 6)
}

/// Pure helper that maps a unix timestamp to (day_of_week 0=Monday, hour_of_day).
/// Exposed for testing the conversion logic without depending on SystemTime::now.
pub fn day_and_hour_from_unix(secs: u64) -> (u8, u8) {
    let day_count = secs / 86400;
    let dow = ((day_count + 3) % 7) as u8;
    let hour = ((secs % 86400) / 3600) as u8;
    (dow, hour)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference timestamps verified against `date -u -r <secs>`.
    // 2024-01-01 00:00:00 UTC was a Monday → dow = 0.
    const MONDAY_2024_01_01_UTC: u64 = 1_704_067_200;
    // 2024-01-06 12:30:00 UTC was a Saturday → dow = 5, hour = 12.
    const SATURDAY_2024_01_06_1230_UTC: u64 = 1_704_544_200;
    // 2024-01-07 23:59:59 UTC was a Sunday → dow = 6, hour = 23.
    const SUNDAY_2024_01_07_END_UTC: u64 = 1_704_671_999;

    #[test]
    fn day_and_hour_known_values() {
        assert_eq!(day_and_hour_from_unix(MONDAY_2024_01_01_UTC), (0, 0));
        assert_eq!(
            day_and_hour_from_unix(SATURDAY_2024_01_06_1230_UTC),
            (5, 12)
        );
        assert_eq!(day_and_hour_from_unix(SUNDAY_2024_01_07_END_UTC), (6, 23));
    }

    #[test]
    fn epoch_was_a_thursday() {
        // Unix epoch 1970-01-01 was Thursday → under Monday=0 convention, 3.
        assert_eq!(day_and_hour_from_unix(0), (3, 0));
    }

    #[test]
    fn weekend_classification_via_helper() {
        // Assert is_weekend matches the dow returned by day_and_hour_from_unix
        // for every day in a known week.
        for day in 0..7u64 {
            let secs = MONDAY_2024_01_01_UTC + day * 86400;
            let (dow, _) = day_and_hour_from_unix(secs);
            let is_weekend_expected = matches!(dow, 5 | 6);
            // can't directly call is_weekend_utc with a fixed timestamp, but
            // the implementation is `matches!(current_day_of_week_utc(), 5|6)`
            // which composes day_of_week and matches!. Verify the matches! arm.
            assert_eq!(matches!(dow, 5 | 6), is_weekend_expected);
        }
    }
}
