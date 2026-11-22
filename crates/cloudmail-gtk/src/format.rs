//! Dates and sizes as the window shows them, in the language Omarchy is set to. The command line keeps its own fixed
//! English forms (`cloudmail_api::text`), which scripts and agents read; these are for people, and are written again
//! after a switch of language by whatever shows them.

use gtk::glib;
use omui::i18n::{format_datetime, format_day_month, format_number, format_time_short};

/// A time in a list: the time of day for today, the weekday within the last week, the day and month this year, and
/// the whole date before that. The time of day and the day and month are written as the language writes them (a 12- or
/// 24-hour clock, the day before or after the month); the other two are `strftime` formats and are translated.
pub fn short_time(ms: i64) -> String {
    let (Some(when), Ok(now)) = (local(ms), glib::DateTime::now_local()) else {
        return String::new();
    };
    let age = now.difference(&when).as_seconds();
    if when.ymd() == now.ymd() {
        format_time_short(&when)
    } else if (0..6 * 24 * 3600).contains(&age) {
        format_datetime(&when, &omui::tr("%a"))
    } else if when.year() == now.year() {
        format_day_month(&when)
    } else {
        format_datetime(&when, &omui::tr("%Y-%m-%d"))
    }
}

/// A message's date in full, as its header shows it: the date, and the time of day as the language writes one.
pub fn long_time(ms: i64) -> String {
    local(ms)
        .map(|when| {
            let date = format_datetime(&when, &omui::tr("%a, %b %-d %Y"));
            omui::tr!("{} at {}", date, format_time_short(&when))
        })
        .unwrap_or_default()
}

fn local(ms: i64) -> Option<glib::DateTime> {
    glib::DateTime::from_unix_local(ms.div_euclid(1000)).ok()
}

/// "2 KB", "1.5 MB": the number in the language's digits, the unit as it is everywhere.
pub fn size(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{} MB", format_number(b / 1024.0 / 1024.0, 1))
    } else if b >= 1024.0 {
        format!("{} KB", format_number(b / 1024.0, 0))
    } else {
        format!("{} B", omui::i18n::format_int(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_numbers_in_the_language_with_their_unit() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "2 KB");
        assert_eq!(size(3 * 1024 * 1024 / 2), "1.5 MB");
    }

    #[test]
    fn a_date_in_full_says_the_year_and_the_time() {
        // 2021-06-15 12:00 UTC: whatever the zone the test runs in, it is that year, and has its minutes.
        let text = long_time(1_623_758_400_000);
        assert!(text.contains("2021"), "{text}");
        assert!(text.contains(':'), "{text}");
        assert!(
            short_time(1_623_758_400_000).starts_with("2021-"),
            "a past year is a whole date"
        );
        assert_eq!(short_time(i64::MAX), "", "no such time");
    }
}
