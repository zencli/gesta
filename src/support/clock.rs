//! Calendar arithmetic, so the tool does not depend on a crate that has stopped
//! being maintained.
//!
//! What this program needs from a date library is small: the current second, the
//! year, month, day, hour and minute of a given second, and the inverse for
//! parsing `YYYY-MM-DD`. That is a handful of integer operations, and the
//! algorithm below is the standard one, published and stable for decades.
//!
//! All times are UTC. Timestamps are seconds since the Unix epoch, which is what
//! the database stores.

/// Seconds since the Unix epoch.
pub fn unix_now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Before 1970 cannot happen on a correct clock. Clamping is better than
        // returning an error from a function called everywhere.
        Err(_) => 0,
    }
}

/// Days since 1970-01-01 and the second within that day.
fn split(timestamp: i64) -> (i64, i64) {
    // Floor division: a negative timestamp must move to the previous day, not
    // toward zero, or 1969-12-31T23:59:59 becomes 1970-01-01 minus one second.
    let days = timestamp.div_euclid(86_400);
    let secs = timestamp.rem_euclid(86_400);
    (days, secs)
}

/// Civil date from a day count, by Howard Hinnant's algorithm.
///
/// It shifts the epoch to 0000-03-01 so that leap days land at the end of the
/// year, which removes every special case from the month arithmetic.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Day count from a civil date, the inverse of `civil_from_days`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// `YYYY-MM-DD HH:MM`, in UTC.
pub fn format_minute(timestamp: i64) -> String {
    let (days, secs) = split(timestamp);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the RFC 3339 form used in log lines.
pub fn format_rfc3339(timestamp: i64) -> String {
    let (days, secs) = split(timestamp);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Start of the day, in seconds since the epoch.
pub fn day_start(y: i64, m: u32, d: u32) -> i64 {
    days_from_civil(y, m, d) * 86_400
}

/// Parse `YYYY-MM-DD` as the start of that day.
///
/// A date that does not exist returns `None` rather than being clamped: a caller
/// asking for 2026-02-30 has a bug, and silently turning it into 2026-03-02 would
/// hide it.
pub fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if m == 0 || m > 12 || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some(day_start(y, m, d))
}

/// Parse `YYYY-MM-DD HH:MM` or `YYYY-MM-DDTHH:MM`, as UTC.
pub fn parse_minute(s: &str) -> Option<i64> {
    let s = s.trim();
    let sep = if s.contains('T') { 'T' } else { ' ' };
    let (date, time) = s.split_once(sep)?;
    let base = parse_date(date)?;
    let tb: Vec<&str> = time.trim_end_matches('Z').split(':').collect();
    if tb.len() < 2 {
        return None;
    }
    let h: i64 = tb[0].parse().ok()?;
    let mi: i64 = tb[1].parse().ok()?;
    let sec: i64 = if tb.len() > 2 {
        tb[2].split('.').next()?.parse().ok()?
    } else {
        0
    };
    // Both ends (#303). `parse::<i64>` accepts a leading minus, so `-1:30` parsed successfully and
    // produced a timestamp an hour and a half before midnight of that day: silently wrong rather than
    // refused. `parse_date` next door already rejects month and day 0; this is the same rule on the
    // clock, and `sec` allows 60 because a leap second is a real value.
    if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&sec) {
        return None;
    }
    Some(base + h * 3600 + mi * 60 + sec)
}

/// End of the day containing this date, as the last second.
pub fn end_of_day(s: &str) -> Option<i64> {
    parse_date(s).map(|d| d + 86_399)
}

/// `YYYYMMDDTHHMMSSZ`, the form S3 signatures use for x-amz-date.
pub fn format_amz_date(timestamp: i64) -> String {
    let (days, secs) = split(timestamp);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_its_own_day() {
        assert_eq!(format_minute(0), "1970-01-01 00:00");
        assert_eq!(day_start(1970, 1, 1), 0);
    }

    #[test]
    fn a_known_timestamp_matches_a_known_date() {
        // 2021-01-01T00:00:00Z is 1609459200. Checked against the published value,
        // not against this code.
        assert_eq!(day_start(2021, 1, 1), 1_609_459_200);
        assert_eq!(format_rfc3339(1_609_459_200), "2021-01-01T00:00:00Z");
    }

    #[test]
    fn leap_days_are_handled_in_both_directions() {
        // 2020 is a leap year, 1900 is not, 2000 is.
        assert_eq!(days_in_month(2020, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(format_minute(day_start(2020, 2, 29)), "2020-02-29 00:00");
        // The day after 2020-02-28 is the 29th, not the 1st of March.
        assert_eq!(
            format_minute(day_start(2020, 2, 28) + 86_400),
            "2020-02-29 00:00"
        );
        // The day after 2019-02-28 is the 1st of March.
        assert_eq!(
            format_minute(day_start(2019, 2, 28) + 86_400),
            "2019-03-01 00:00"
        );
    }

    #[test]
    fn every_day_round_trips_through_the_pair_of_conversions() {
        // A month at each year boundary, the leap day and the epoch, converted to
        // a day count and back. A single off-by-one in either direction shows up
        // here as a date that does not match itself.
        let mut checked = 0;
        for (y, m, d) in [
            (1970, 1, 1),
            (1969, 12, 31),
            (2000, 2, 29),
            (2026, 11, 17),
            (1900, 3, 1),
            (2100, 12, 31),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "{} - {} - {}", y, m, d);
            checked += 1;
        }
        assert_eq!(checked, 6);
    }

    #[test]
    fn a_negative_timestamp_lands_on_the_previous_day() {
        // One second before the epoch is 1969-12-31 23:59:59, not 1970-01-01.
        assert_eq!(format_rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_minute(-1), "1969-12-31 23:59");
    }

    #[test]
    fn parsing_rejects_a_date_that_does_not_exist() {
        assert!(parse_date("2026-02-30").is_none());
        assert!(parse_date("2026-13-01").is_none());
        assert!(parse_date("2026-00-10").is_none());
        assert!(parse_date("2026-1").is_none());
        assert!(parse_date("not a date").is_none());
        assert!(
            parse_date("2024-02-29").is_some(),
            "a real leap day is accepted"
        );
    }

    #[test]
    fn a_parsed_date_round_trips_through_the_formatter() {
        for s in ["2026-11-17", "2024-02-29", "1970-01-01", "2099-12-31"] {
            let ts = parse_date(s).expect("should parse");
            assert_eq!(&format_minute(ts)[..10], s);
        }
    }

    #[test]
    fn a_minute_is_parsed_with_either_separator() {
        let a = parse_minute("2026-11-17 08:30").unwrap();
        let b = parse_minute("2026-11-17T08:30").unwrap();
        assert_eq!(a, b);
        assert_eq!(format_minute(a), "2026-11-17 08:30");
    }

    #[test]
    fn the_end_of_a_day_is_the_last_second() {
        let start = parse_date("2026-11-17").unwrap();
        let end = end_of_day("2026-11-17").unwrap();
        assert_eq!(end - start, 86_399);
        assert_eq!(format_rfc3339(end), "2026-11-17T23:59:59Z");
    }
    #[test]
    fn a_negative_clock_field_is_refused_rather_than_silently_wrong() {
        // #303: only the upper bounds were checked, and `parse::<i64>` accepts a leading minus, so
        // `2026-11-17 -1:30` parsed successfully and produced a timestamp an hour and a half before
        // midnight of that day. Silently wrong is worse than refused: nothing downstream can tell.
        assert!(
            parse_minute("2026-11-17 -1:30").is_none(),
            "a negative hour must be refused"
        );
        assert!(
            parse_minute("2026-11-17 12:-30").is_none(),
            "a negative minute must be refused"
        );
        assert!(
            parse_minute("2026-11-17 12:30:-5").is_none(),
            "a negative second must be refused"
        );

        // The valid forms still parse, so the guard did not become a wall.
        assert!(
            parse_minute("2026-11-17 00:00").is_some(),
            "midnight is valid"
        );
        assert!(
            parse_minute("2026-11-17 23:59:60").is_some(),
            "23:59:60 is a leap second and stays valid"
        );
    }
}
