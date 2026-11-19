//! Wall-clock time for logs and file names.

/// UTC timestamp like `2026-11-10T13:05:09Z`.
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_utc(secs)
}

/// UTC time down to the millisecond for file names, like
/// `20261110T130509123Z`: two recordings started in one second keep apart.
pub fn file_stamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = format_utc(now.as_secs() as i64).replace([':', '-'], "");
    format!("{}{:03}Z", secs.trim_end_matches('Z'), now.subsec_millis())
}

pub fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_794_316_509), "2026-11-10T13:15:09Z");
    }

    #[test]
    fn file_stamp_has_milliseconds() {
        let stamp = file_stamp();
        assert_eq!(stamp.len(), 19, "{stamp}");
        assert!(stamp.ends_with('Z') && stamp.chars().filter(char::is_ascii_digit).count() == 17, "{stamp}");
    }
}
