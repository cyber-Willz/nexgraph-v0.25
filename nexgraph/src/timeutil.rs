//! Time helpers shared by the always-on analytics and the (optional) importers.

/// Seconds since the Unix epoch of an RFC 3339 / ISO 8601 timestamp (`2026-11-14T10:00:00.123456+0000`,
/// `...Z`, `...+01:00`; a space may replace `T`; a bare date `2025-10-16` is midnight UTC). Returns `None` if it does not parse,
/// including days that do not exist (`2025-02-30`).
pub fn parse_timestamp(s: &str) -> Option<f64> {
    let s = s.trim();
    // A bare date (`2025-10-16`) is midnight UTC; with a time it needs `T` or a space between the two.
    let (date, rest) = match s.split_once(['T', ' ']) {
        Some((d, r)) => (d, Some(r)),
        None => (s, None),
    };
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    if d.next().is_some() || !(1..=12).contains(&m) || day < 1 || day > days_in_month(y, m) {
        return None;
    }
    let rest = rest.unwrap_or("00:00");
    let (time, off) = match rest.find(['+', '-', 'Z']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let mut t = time.split(':');
    let (h, mi): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    if h > 23 || mi > 59 || !(0.0..61.0).contains(&sec) {
        return None;
    }
    let off_s = if off.is_empty() || off == "Z" {
        0
    } else {
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let digits: String = off[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() < 2 {
            return None;
        }
        let hh: i64 = digits[..2].parse().ok()?;
        let mm: i64 = if digits.len() >= 4 { digits[2..4].parse().ok()? } else { 0 };
        sign * (hh * 3600 + mm * 60)
    };
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 - off_s) as f64 + sec)
}

/// How a column's dates are written. ISO 8601 (`2025-10-16`) is read as-is; `11/8/2016` is month-first in the US
/// and day-first elsewhere and cannot be told apart, so the order must be declared, never guessed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateOrder {
    #[default]
    Iso,
    /// `M/D/YYYY` (also `-` or `.` separators)
    MonthFirst,
    /// `D/M/YYYY`
    DayFirst,
}

impl DateOrder {
    pub fn parse(s: &str) -> Option<DateOrder> {
        match s.trim().to_ascii_lowercase().as_str() {
            "iso" | "ymd" => Some(DateOrder::Iso),
            "mdy" | "us" => Some(DateOrder::MonthFirst),
            "dmy" | "eu" => Some(DateOrder::DayFirst),
            _ => None,
        }
    }
}

/// [`parse_timestamp`] for a declared date order. Non-ISO orders accept `M/D/YYYY` or `D/M/YYYY` (separator `/`,
/// `-` or `.`; a four-digit year is required) optionally followed by `H:MM[:SS]` and `AM`/`PM`.
pub fn parse_timestamp_ordered(s: &str, order: DateOrder) -> Option<f64> {
    if order == DateOrder::Iso {
        return parse_timestamp(s);
    }
    let s = s.trim();
    let (date, rest) = match s.split_once(' ') {
        Some((d, r)) => (d, r.trim()),
        None => (s, ""),
    };
    let parts: Vec<&str> = date.split(['/', '-', '.']).collect();
    if parts.len() != 3 || parts[2].len() != 4 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let n: Vec<i64> = parts.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let (m, d, y) = if order == DateOrder::MonthFirst { (n[0], n[1], n[2]) } else { (n[1], n[0], n[2]) };
    let (mut time, mut pm) = (rest.to_string(), None);
    let up = rest.to_ascii_uppercase();
    for (suf, is_pm) in [("AM", false), ("PM", true)] {
        if let Some(t) = up.strip_suffix(suf) {
            time = t.trim().to_string();
            pm = Some(is_pm);
        }
    }
    if time.is_empty() {
        time = "00:00:00".into();
    }
    if let Some(is_pm) = pm {
        let mut t = time.splitn(2, ':');
        let h: i64 = t.next()?.trim().parse().ok()?;
        if !(1..=12).contains(&h) {
            return None;
        }
        let h24 = (h % 12) + if is_pm { 12 } else { 0 };
        time = format!("{h24:02}:{}", t.next().unwrap_or("00"));
    }
    parse_timestamp(&format!("{y:04}-{m:02}-{d:02} {time}"))
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A timestamp property as epoch seconds: a finite number is taken as-is (seconds), a string is parsed as
/// ISO 8601 / RFC 3339 (see [`parse_timestamp`]). Anything else is `None`.
pub fn epoch_seconds(v: &crate::value::Value) -> Option<f64> {
    use crate::value::Value;
    match v {
        Value::Str(s) => parse_timestamp(s),
        Value::Int(_) | Value::Float(_) => v.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

#[cfg(test)]
mod date_tests {
    use super::parse_timestamp;

    #[test]
    fn bare_dates_are_midnight_utc() {
        assert_eq!(parse_timestamp("1970-01-01"), Some(0.0));
        assert_eq!(parse_timestamp("2025-10-16"), parse_timestamp("2025-10-16 00:00:00"));
        assert_eq!(parse_timestamp(" 2025-10-16T00:00:00Z "), parse_timestamp("2025-10-16"));
        assert_eq!(parse_timestamp("2000-03-01").unwrap() - parse_timestamp("2000-02-29").unwrap(), 86_400.0);
    }

    #[test]
    fn impossible_or_ambiguous_dates_are_refused() {
        for bad in ["2025-02-30", "2023-02-29", "2025-04-31", "2025-13-01", "2025-00-10", "2025-10-00", "16/10/2025", "10/16/2025", "2025", "2025-10", "yesterday", ""] {
            assert_eq!(parse_timestamp(bad), None, "{bad:?}");
        }
        assert!(parse_timestamp("2024-02-29").is_some() && parse_timestamp("2000-02-29").is_some() && parse_timestamp("1900-02-29").is_none());
    }

    #[test]
    fn declared_date_orders() {
        use super::{parse_timestamp_ordered as p, DateOrder::*};
        let iso = |s| parse_timestamp(s);
        assert_eq!(p("11/8/2016", MonthFirst), iso("2016-11-08"));
        assert_eq!(p("11/8/2016", DayFirst), iso("2016-08-11"));
        assert_eq!(p("9/22/2025", MonthFirst), iso("2025-09-22"));
        assert_eq!(p("9/22/2025", DayFirst), None, "month 22 does not exist");
        assert_eq!(p("22.09.2025", DayFirst), iso("2025-09-22"));
        assert_eq!(p("3/5/2024 1:05 PM", MonthFirst), iso("2024-03-05 13:05"));
        assert_eq!(p("3/5/2024 12:00 AM", MonthFirst), iso("2024-03-05 00:00"));
        assert_eq!(p("3/5/2024 12:30 PM", MonthFirst), iso("2024-03-05 12:30"));
        assert_eq!(p("3/5/2024 14:30:10", MonthFirst), iso("2024-03-05 14:30:10"));
        assert_eq!(p("2025-10-16", Iso), iso("2025-10-16"));
        for bad in ["2/30/2024", "3/5/24", "13/1/2024", "3/5/2024 13:00 PM", "3/5/2024 0:30 AM", "a/b/cccc", "3/5", ""] {
            assert_eq!(p(bad, MonthFirst), None, "{bad:?}");
        }
        assert_eq!(super::DateOrder::parse("MDY"), Some(MonthFirst));
        assert_eq!(super::DateOrder::parse("xx"), None);
    }

    #[test]
    fn times_and_offsets_still_work() {
        assert_eq!(parse_timestamp("2025-10-16 12:30:15.5"), Some(parse_timestamp("2025-10-16").unwrap() + 45_015.5));
        assert_eq!(parse_timestamp("2025-10-16T12:00:00+02:00"), Some(parse_timestamp("2025-10-16T10:00:00Z").unwrap()));
    }
}
