use chrono::{Local, NaiveDate, NaiveTime, TimeDelta};

use crate::error::{AppError, Result};

/// Parse flexible date inputs into NaiveDate.
///
/// Supports:
/// - ISO format: "2025-11-13"
/// - Relative: "yesterday", "today"
pub fn parse_date(date_str: &str) -> Result<NaiveDate> {
    let lower = date_str.trim().to_lowercase();

    match lower.as_str() {
        "yesterday" => Ok(Local::now().date_naive() - TimeDelta::days(1)),
        "today" => Ok(Local::now().date_naive()),
        _ => NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d").map_err(|_| {
            AppError::DateParse(format!(
                "Invalid date format: '{}'. Use YYYY-MM-DD, 'today', or 'yesterday'",
                date_str
            ))
        }),
    }
}

/// Build SQL WHERE clause and parameters for date filtering.
///
/// Returns (sql_clause, params) tuple. Empty string if no filters.
pub fn build_date_filter(
    since: Option<&str>,
    until: Option<&str>,
    date: Option<&str>,
) -> Result<(String, Vec<String>)> {
    if let Some(date_str) = date {
        let start = parse_date(date_str)?;
        let end = start + TimeDelta::days(1);
        let start_dt = start.and_time(NaiveTime::MIN);
        let end_dt = end.and_time(NaiveTime::MIN);
        return Ok((
            "timestamp >= ? AND timestamp < ?".to_string(),
            vec![
                start_dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
                end_dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
            ],
        ));
    }

    let mut clauses = Vec::new();
    let mut params = Vec::new();

    if let Some(since_str) = since {
        let start = parse_date(since_str)?;
        let start_dt = start.and_time(NaiveTime::MIN);
        clauses.push("timestamp >= ?".to_string());
        params.push(start_dt.format("%Y-%m-%dT%H:%M:%S").to_string());
    }

    if let Some(until_str) = until {
        let end = parse_date(until_str)? + TimeDelta::days(1);
        let end_dt = end.and_time(NaiveTime::MIN);
        clauses.push("timestamp < ?".to_string());
        params.push(end_dt.format("%Y-%m-%dT%H:%M:%S").to_string());
    }

    if clauses.is_empty() {
        return Ok(("".to_string(), vec![]));
    }

    Ok((clauses.join(" AND "), params))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_build_date_filter_single_date() {
        let (sql, params) = build_date_filter(None, None, Some("2025-11-13")).unwrap();
        assert_eq!(sql, "timestamp >= ? AND timestamp < ?");
        assert_eq!(params[0], "2025-11-13T00:00:00");
        assert_eq!(params[1], "2025-11-14T00:00:00");
    }

    #[test]
    fn test_build_date_filter_since_until() {
        let (sql, params) = build_date_filter(Some("2025-11-10"), Some("2025-11-13"), None).unwrap();
        assert_eq!(sql, "timestamp >= ? AND timestamp < ?");
        assert_eq!(params[0], "2025-11-10T00:00:00");
        assert_eq!(params[1], "2025-11-14T00:00:00");
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
        let (sql, params) = build_date_filter(None, None, Some("2024-12-31")).unwrap();
        assert_eq!(sql, "timestamp >= ? AND timestamp < ?");
        assert_eq!(params[0], "2024-12-31T00:00:00");
        assert_eq!(params[1], "2025-01-01T00:00:00");
    }

    #[test]
    fn test_build_date_filter_since_only() {
        let (sql, params) = build_date_filter(Some("2025-03-01"), None, None).unwrap();
        assert_eq!(sql, "timestamp >= ?");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], "2025-03-01T00:00:00");
    }
}
