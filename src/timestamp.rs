//! ISO 8601 UTC timestamp helpers.
//!
//! Uses public-domain calendar arithmetic (Hinnant) to avoid pulling in
//! `chrono` or `time` as a direct dependency. Lives in the library rather than
//! the adapter binary so the log sink — shared with the proxy binary — can
//! stamp records without a second copy of the calendar maths.

use std::time::{SystemTime, UNIX_EPOCH};

const EPOCH_FALLBACK_SECONDS: &str = "1970-01-01T00:00:00Z";
const EPOCH_FALLBACK_MILLIS: &str = "1970-01-01T00:00:00.000Z";

/// Broken-down UTC calendar fields.
struct UtcParts {
    year: u64,
    month: u64,
    day: u64,
    hours: u64,
    minutes: u64,
    seconds: u64,
}

/// Produce an ISO 8601 UTC timestamp string from the system clock.
///
/// The format is `YYYY-MM-DDTHH:MM:SSZ` with second precision. A clock set
/// before the Unix epoch yields the epoch itself rather than failing.
#[must_use]
pub fn iso_timestamp_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or_else(
            || EPOCH_FALLBACK_SECONDS.to_string(),
            |dur| {
                let parts = split_utc(dur.as_secs());
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                    parts.year, parts.month, parts.day, parts.hours, parts.minutes, parts.seconds
                )
            },
        )
}

/// Produce an ISO 8601 UTC timestamp string with millisecond precision.
///
/// The format is `YYYY-MM-DDTHH:MM:SS.sssZ`. Log records use this rather than
/// the second-precision form because a busy session emits many records per
/// second and their relative order has to stay readable.
#[must_use]
pub fn iso_timestamp_millis_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or_else(
            || EPOCH_FALLBACK_MILLIS.to_string(),
            |dur| {
                let parts = split_utc(dur.as_secs());
                let millis = dur.subsec_millis();
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{millis:03}Z",
                    parts.year, parts.month, parts.day, parts.hours, parts.minutes, parts.seconds
                )
            },
        )
}

/// Split seconds since the Unix epoch into UTC calendar fields.
fn split_utc(secs: u64) -> UtcParts {
    let days = secs / 86400;
    let seconds_today = secs % 86400;
    let (year, month, day) = unix_days_to_ymd(days);

    UtcParts {
        year,
        month,
        day,
        hours: seconds_today / 3600,
        minutes: (seconds_today % 3600) / 60,
        seconds: seconds_today % 60,
    }
}

fn unix_days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    days += 719_468; // Adjust to proleptic Gregorian calendar
    let era = days / 146_097;
    let day_of_era = days % 146_097;

    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;

    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);

    let month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month + 2) / 5 + 1;

    let month = if month < 10 { month + 3 } else { month - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    (year, month, day)
}

#[cfg(test)]
mod tests;
