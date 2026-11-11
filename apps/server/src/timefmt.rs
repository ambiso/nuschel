// SPDX-License-Identifier: EUPL-1.2
//! UTC timestamps in the `YYYY-MM-DDTHH:MM:SS` form Mumble uses for ban starts
//! and last-seen times (Qt's `Qt::ISODate`).

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

pub fn iso8601(unix: i64) -> String {
    let (days, seconds) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Accepts `YYYY-MM-DDTHH:MM:SS` with an optional `Z` or fractional seconds.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    let text = text.trim().trim_end_matches('Z');
    let text = text.split('.').next()?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let mut time = time.split(':').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let (hour, minute, second) = (
        time.next()??,
        time.next()??,
        time.next().unwrap_or(Some(0))?,
    );
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..61).contains(&second)
        || !(0..=9999).contains(&year)
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_known_values() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00");
        assert_eq!(iso8601(1_762_819_200), "2025-11-11T00:00:00");
        assert_eq!(parse_iso8601("2025-11-11T00:00:00"), Some(1_762_819_200));
        assert_eq!(parse_iso8601("2024-02-29T23:59:59Z"), Some(1_709_251_199));
        for value in [0, 951_782_400, 4_102_444_800, 1_234_567_890] {
            assert_eq!(parse_iso8601(&iso8601(value)), Some(value));
        }
        for bad in [
            "",
            "T",
            "2024-13-01T00:00:00",
            "x-y-zTa:b:c",
            "2024-01-01",
            "99999-01-01T00:00:00",
        ] {
            assert_eq!(parse_iso8601(bad), None, "{bad}");
        }
    }
}
