//! Reading a time the user wrote: absolute RFC3339 or a relative offset.
//!
//! Pure. The current time is a parameter, not a call — the decision "when is
//! this due" is made by comparing the returned instant with a `now` its caller
//! supplies, which is what keeps every scheduled decision reproducible from a
//! log and a clock value.

use chrono::{DateTime, Duration, Utc};

/// Why a `when` string could not be read.
///
/// Kept typed rather than a sentence because the CLI prints it and tests assert
/// on the category; collapsing it to `String` would leave a caller parsing
/// prose to tell "you forgot the unit" from "that is not a time at all".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("`{0}` is not an RFC3339 timestamp or a relative offset like `+10m`")]
    Unreadable(String),
    #[error("relative offset `{0}` has no unit; use s, m, h, d, or w")]
    MissingUnit(String),
    #[error("relative offset `{0}` uses unknown unit `{1}`; use s, m, h, d, or w")]
    UnknownUnit(String, String),
    #[error("relative offset `{0}` is too large to represent")]
    Overflow(String),
}

/// Read `input` as an instant, relative to `now`.
///
/// Accepted forms:
///
/// - RFC3339 with an offset, e.g. `2026-10-18T09:30:00+08:00`.
/// - `+<n><unit>`, where the unit is `s`, `m`, `h`, `d`, or `w`. `+0s` means
///   "now" and is accepted on purpose: scheduling an immediate turn is a
///   legitimate request, and refusing zero would make callers special-case it.
///
/// A bare `10m`, a negative `-10m`, and a decimal `+1.5h` are all refused. The
/// first two are refused because `+` is the thing that distinguishes "an
/// offset from now" from "a date"; the third because the unit grammar has no
/// fractions, and silently truncating one would schedule a different instant
/// than the one written.
pub fn parse_when(input: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, ScheduleError> {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix('+') else {
        return DateTime::parse_from_rfc3339(trimmed)
            .map(|when| when.with_timezone(&Utc))
            .map_err(|_| ScheduleError::Unreadable(trimmed.to_string()));
    };

    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Err(ScheduleError::Unreadable(trimmed.to_string()));
    }
    let unit = &rest[digits.len()..];
    if unit.is_empty() {
        return Err(ScheduleError::MissingUnit(trimmed.to_string()));
    }
    let seconds_per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        other => {
            return Err(ScheduleError::UnknownUnit(trimmed.to_string(), other.to_string()));
        }
    };

    let count: i64 = digits.parse().map_err(|_| ScheduleError::Overflow(trimmed.to_string()))?;
    let seconds = count
        .checked_mul(seconds_per_unit)
        .ok_or_else(|| ScheduleError::Overflow(trimmed.to_string()))?;
    now.checked_add_signed(Duration::seconds(seconds))
        .ok_or_else(|| ScheduleError::Overflow(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-18T09:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn an_absolute_time_with_an_offset_is_that_instant() {
        let when = parse_when("2026-10-18T17:30:00+08:00", now()).unwrap();
        assert_eq!(when, DateTime::parse_from_rfc3339("2026-10-18T09:30:00Z").unwrap());
    }

    #[test]
    fn relative_offsets_add_to_now() {
        assert_eq!(parse_when("+0s", now()).unwrap(), now());
        assert_eq!(parse_when("+90s", now()).unwrap(), now() + Duration::seconds(90));
        assert_eq!(parse_when("+10m", now()).unwrap(), now() + Duration::minutes(10));
        assert_eq!(parse_when("+2h", now()).unwrap(), now() + Duration::hours(2));
        assert_eq!(parse_when("+1d", now()).unwrap(), now() + Duration::days(1));
        assert_eq!(parse_when("+1w", now()).unwrap(), now() + Duration::weeks(1));
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_the_time() {
        assert_eq!(parse_when("  +5m  ", now()).unwrap(), now() + Duration::minutes(5));
    }

    #[test]
    fn a_relative_offset_without_a_plus_is_refused() {
        // `10m` is not a date and is not spelled as an offset; reading it as
        // either would guess which one the caller meant.
        assert!(matches!(parse_when("10m", now()), Err(ScheduleError::Unreadable(_))));
        assert!(matches!(parse_when("-10m", now()), Err(ScheduleError::Unreadable(_))));
    }

    #[test]
    fn a_relative_offset_needs_a_unit_and_a_known_one() {
        assert!(matches!(parse_when("+10", now()), Err(ScheduleError::MissingUnit(_))));
        assert!(matches!(
            parse_when("+10x", now()),
            Err(ScheduleError::UnknownUnit(_, unit)) if unit == "x"
        ));
        assert!(
            matches!(parse_when("+1.5h", now()), Err(ScheduleError::UnknownUnit(_, _))),
            "fractions are not part of the grammar"
        );
        assert!(matches!(parse_when("+", now()), Err(ScheduleError::Unreadable(_))));
    }

    #[test]
    fn an_offset_too_large_to_represent_is_refused_not_wrapped() {
        assert!(matches!(
            parse_when("+999999999999999999999999w", now()),
            Err(ScheduleError::Overflow(_))
        ));
    }
}
