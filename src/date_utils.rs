use chrono::{DateTime, Datelike, Days, Local, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};

use crate::error::{AppError, Result};

/// Format of range bounds. Stored timestamps are UTC `YYYY-MM-DDTHH:MM:SS.sssZ`;
/// a bound without fraction or `Z` sorts before every stored value of the same
/// second, so `>= start` includes it and `< end` excludes it.
const BOUND_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

/// Parse flexible date inputs into NaiveDate.
///
/// Supports:
/// - ISO format: "2025-11-13"
/// - Relative: "yesterday", "today"
///
/// Years are limited to 1..=9999: a bound outside 4 digits formats as `+10000-...`
/// or `-0001-...`, which no longer sorts against stored timestamps.
pub fn parse_date(date_str: &str) -> Result<NaiveDate> {
    let lower = date_str.trim().to_lowercase();

    let date = match lower.as_str() {
        "yesterday" => Local::now()
            .date_naive()
            .pred_opt()
            .ok_or_else(|| AppError::DateParse("'yesterday' is out of range".to_string()))?,
        "today" => Local::now().date_naive(),
        _ => NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d").map_err(|_| {
            AppError::DateParse(format!(
                "Invalid date format: '{}'. Use YYYY-MM-DD, 'today', or 'yesterday'",
                date_str
            ))
        })?,
    };
    if !(1..=9999).contains(&date.year()) {
        return Err(AppError::DateParse(format!(
            "Date out of range: '{}'. Years must be between 0001 and 9999",
            date_str
        )));
    }
    Ok(date)
}

/// UTC bound for 00:00 of `date` in `tz`. `None` when the instant falls past
/// year 9999 and cannot be written as a comparable bound.
fn local_midnight_utc<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> Result<Option<String>> {
    let utc = date
        .and_time(NaiveTime::MIN)
        .and_local_timezone(tz.clone())
        .earliest()
        .ok_or_else(|| {
            AppError::DateParse(format!(
                "{} 00:00 does not exist in the local timezone",
                date
            ))
        })?
        .with_timezone(&Utc);
    if utc.year() > 9999 {
        return Ok(None);
    }
    Ok(Some(utc.format(BOUND_FORMAT).to_string()))
}

/// UTC bound for the end of `date` in `tz` (00:00 of the following day).
fn local_day_end_utc<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> Result<Option<String>> {
    let next = date
        .checked_add_days(Days::new(1))
        .ok_or_else(|| AppError::DateParse(format!("Date out of range: {}", date)))?;
    local_midnight_utc(next, tz)
}

/// Resolve `--date` / `--since` / `--until` into a half-open UTC range
/// `[start, end)`. `date` takes precedence over `since`/`until`. Days are local
/// calendar days; the bounds are UTC because that is what the index stores.
pub fn date_bounds(
    since: Option<&str>,
    until: Option<&str>,
    date: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    date_bounds_in(since, until, date, &Local)
}

fn date_bounds_in<Tz: TimeZone>(
    since: Option<&str>,
    until: Option<&str>,
    date: Option<&str>,
    tz: &Tz,
) -> Result<(Option<String>, Option<String>)> {
    if let Some(d) = date {
        let day = parse_date(d)?;
        return Ok((local_midnight_utc(day, tz)?, local_day_end_utc(day, tz)?));
    }
    let start = match since {
        Some(s) => local_midnight_utc(parse_date(s)?, tz)?,
        None => None,
    };
    let end = match until {
        Some(u) => local_day_end_utc(parse_date(u)?, tz)?,
        None => None,
    };
    Ok((start, end))
}

/// Build SQL WHERE clause and parameters for date filtering.
///
/// Returns (sql_clause, params) tuple. Empty string if no filters.
pub fn build_date_filter(
    since: Option<&str>,
    until: Option<&str>,
    date: Option<&str>,
) -> Result<(String, Vec<String>)> {
    build_date_filter_in(since, until, date, &Local)
}

fn build_date_filter_in<Tz: TimeZone>(
    since: Option<&str>,
    until: Option<&str>,
    date: Option<&str>,
    tz: &Tz,
) -> Result<(String, Vec<String>)> {
    let (start, end) = date_bounds_in(since, until, date, tz)?;
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    if let Some(s) = start {
        clauses.push("timestamp >= ?");
        params.push(s);
    }
    if let Some(e) = end {
        clauses.push("timestamp < ?");
        params.push(e);
    }
    Ok((clauses.join(" AND "), params))
}

/// UTC bound for `--days N`: now minus N days.
pub fn days_cutoff(days: i64) -> Result<String> {
    days_cutoff_at(Utc::now(), days)
}

fn days_cutoff_at(now: DateTime<Utc>, days: i64) -> Result<String> {
    TimeDelta::try_days(days)
        .and_then(|delta| now.checked_sub_signed(delta))
        .map(|dt| dt.format(BOUND_FORMAT).to_string())
        .ok_or_else(|| AppError::DateParse(format!("--days {} is out of range", days)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn test_parse_date_iso() {
        let d = parse_date("2025-11-13").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2025, 11, 13).unwrap());
    }

    #[test]
    fn test_parse_date_today() {
        let d = parse_date("today").unwrap();
        assert_eq!(d, Local::now().date_naive());
    }

    #[test]
    fn test_parse_date_yesterday() {
        let d = parse_date("yesterday").unwrap();
        assert_eq!(d, Local::now().date_naive() - TimeDelta::days(1));
    }

    #[test]
    fn test_parse_date_invalid() {
        assert!(parse_date("not-a-date").is_err());
    }

    fn jst() -> FixedOffset {
        FixedOffset::east_opt(9 * 3600).unwrap()
    }

    fn est() -> FixedOffset {
        FixedOffset::west_opt(5 * 3600).unwrap()
    }

    #[test]
    fn test_build_date_filter_single_date() {
        let (sql, params) = build_date_filter_in(None, None, Some("2025-11-13"), &Utc).unwrap();
        assert_eq!(sql, "timestamp >= ? AND timestamp < ?");
        assert_eq!(params[0], "2025-11-13T00:00:00");
        assert_eq!(params[1], "2025-11-14T00:00:00");
    }

    #[test]
    fn test_build_date_filter_since_until() {
        let (sql, params) =
            build_date_filter_in(Some("2025-11-10"), Some("2025-11-13"), None, &Utc).unwrap();
        assert_eq!(sql, "timestamp >= ? AND timestamp < ?");
        assert_eq!(params[0], "2025-11-10T00:00:00");
        assert_eq!(params[1], "2025-11-14T00:00:00");
    }

    #[test]
    fn test_date_bounds_are_local_midnights_in_utc() {
        // Stored timestamps are UTC, so a JST day starts at 15:00Z the day before.
        let (start, end) = date_bounds_in(None, None, Some("2025-11-13"), &jst()).unwrap();
        assert_eq!(start.as_deref(), Some("2025-11-12T15:00:00"));
        assert_eq!(end.as_deref(), Some("2025-11-13T15:00:00"));

        let (start, end) = date_bounds_in(None, None, Some("2025-11-13"), &est()).unwrap();
        assert_eq!(start.as_deref(), Some("2025-11-13T05:00:00"));
        assert_eq!(end.as_deref(), Some("2025-11-14T05:00:00"));
    }

    #[test]
    fn test_date_bounds_since_until_in_utc() {
        let (start, end) =
            date_bounds_in(Some("2025-11-10"), Some("2025-11-13"), None, &jst()).unwrap();
        assert_eq!(start.as_deref(), Some("2025-11-09T15:00:00"));
        assert_eq!(end.as_deref(), Some("2025-11-13T15:00:00"));

        let (start, end) = date_bounds_in(None, Some("2025-11-13"), None, &jst()).unwrap();
        assert_eq!(start, None);
        assert_eq!(end.as_deref(), Some("2025-11-13T15:00:00"));
    }

    #[test]
    fn test_date_bound_sorts_against_stored_format() {
        // The bound has no fraction or `Z`; stored values do. String comparison must
        // still put a same-second stored value at or after the bound.
        let (start, end) = date_bounds_in(None, None, Some("2025-11-13"), &jst()).unwrap();
        let (start, end) = (start.unwrap(), end.unwrap());
        assert!("2025-11-12T15:00:00.000Z" >= start.as_str());
        assert!("2025-11-12T14:59:59.999Z" < start.as_str());
        assert!("2025-11-13T14:59:59.999Z" < end.as_str());
        assert!("2025-11-13T15:00:00.000Z" >= end.as_str());
    }

    #[test]
    fn test_date_filter_far_future_date_is_error_not_panic() {
        for (since, until, date) in [
            (None, None, Some("+262142-12-31")),
            (None, Some("+262142-12-31"), None),
            (Some("+262142-12-31"), None, None),
        ] {
            assert!(
                matches!(
                    build_date_filter(since, until, date),
                    Err(AppError::DateParse(_))
                ),
                "{since:?} {until:?} {date:?}"
            );
        }
    }

    #[test]
    fn test_parse_date_rejects_years_outside_four_digits() {
        // A 5-digit year formats as `+10000-...`, which sorts before every stored value.
        assert!(parse_date("+10000-01-01").is_err());
        assert!(parse_date("0000-12-31").is_err());
        assert!(parse_date("9999-12-31").is_ok());
        assert!(parse_date("0001-01-01").is_ok());
    }

    #[test]
    fn test_until_last_representable_day_has_no_upper_bound() {
        // The day after 9999-12-31 is not a 4-digit year; the range is open-ended instead.
        let (start, end) = date_bounds_in(None, None, Some("9999-12-31"), &Utc).unwrap();
        assert_eq!(start.as_deref(), Some("9999-12-31T00:00:00"));
        assert_eq!(end, None);
        let (sql, params) = build_date_filter_in(None, Some("9999-12-31"), None, &Utc).unwrap();
        assert!(sql.is_empty(), "{sql}");
        assert!(params.is_empty());
    }

    #[test]
    fn test_days_cutoff_is_utc_now_minus_days() {
        let now = Utc.with_ymd_and_hms(2025, 11, 13, 3, 4, 5).unwrap();
        assert_eq!(days_cutoff_at(now, 1).unwrap(), "2025-11-12T03:04:05");
        assert_eq!(days_cutoff_at(now, 0).unwrap(), "2025-11-13T03:04:05");
    }

    #[test]
    fn test_days_cutoff_overflow_is_error_not_panic() {
        assert!(matches!(
            days_cutoff(100_000_000),
            Err(AppError::DateParse(_))
        ));
        assert!(matches!(days_cutoff(i64::MAX), Err(AppError::DateParse(_))));
    }

    #[test]
    fn test_build_date_filter_none() {
        let (sql, params) = build_date_filter(None, None, None).unwrap();
        assert!(sql.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn test_parse_date_case_insensitive() {
        let d1 = parse_date("TODAY").unwrap();
        assert_eq!(d1, Local::now().date_naive());

        let d2 = parse_date("Yesterday").unwrap();
        assert_eq!(d2, Local::now().date_naive() - TimeDelta::days(1));
    }

    #[test]
    fn test_parse_date_whitespace() {
        let d = parse_date("  today  ").unwrap();
        assert_eq!(d, Local::now().date_naive());
    }

    #[test]
    fn test_parse_date_leap_year() {
        let d = parse_date("2024-02-29").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2024, 2, 29).unwrap());
    }

    #[test]
    fn test_parse_date_invalid_leap() {
        assert!(parse_date("2023-02-29").is_err());
    }

    #[test]
    fn test_build_date_filter_year_boundary() {
        let (sql, params) = build_date_filter_in(None, None, Some("2024-12-31"), &Utc).unwrap();
        assert_eq!(sql, "timestamp >= ? AND timestamp < ?");
        assert_eq!(params[0], "2024-12-31T00:00:00");
        assert_eq!(params[1], "2025-01-01T00:00:00");
    }

    #[test]
    fn test_build_date_filter_since_only() {
        let (sql, params) = build_date_filter_in(Some("2025-03-01"), None, None, &Utc).unwrap();
        assert_eq!(sql, "timestamp >= ?");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], "2025-03-01T00:00:00");
    }
}
