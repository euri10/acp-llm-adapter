use super::{iso_timestamp_millis_now, iso_timestamp_now, split_utc, unix_days_to_ymd};

#[test]
fn unix_epoch_day_zero_is_1970_01_01() {
    assert_eq!(unix_days_to_ymd(0), (1970, 1, 1));
}

#[test]
fn leap_day_is_resolved_correctly() {
    // 2020-02-29 is 18321 days after the epoch.
    assert_eq!(unix_days_to_ymd(18_321), (2020, 2, 29));
}

#[test]
fn split_utc_breaks_out_time_of_day() {
    // 2020-02-29T13:45:07Z
    let parts = split_utc(18_321 * 86_400 + 13 * 3600 + 45 * 60 + 7);

    assert_eq!(
        (
            parts.year,
            parts.month,
            parts.day,
            parts.hours,
            parts.minutes,
            parts.seconds
        ),
        (2020, 2, 29, 13, 45, 7)
    );
}

#[test]
fn second_precision_timestamp_has_expected_shape() {
    let stamp = iso_timestamp_now();

    assert_eq!(stamp.len(), 20, "unexpected timestamp {stamp}");
    assert!(stamp.ends_with('Z'), "unexpected timestamp {stamp}");
}

#[test]
fn millisecond_precision_timestamp_has_expected_shape() {
    let stamp = iso_timestamp_millis_now();

    assert_eq!(stamp.len(), 24, "unexpected timestamp {stamp}");
    assert!(stamp.ends_with('Z'), "unexpected timestamp {stamp}");
    assert!(
        stamp.chars().nth(19) == Some('.'),
        "unexpected timestamp {stamp}"
    );
}
