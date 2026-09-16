//! Checkpoint construction: UTC calendar arithmetic and commit selection.
//!
//! A checkpoint is a requested cutoff instant plus the commit that was current
//! at that instant, if any. Selection scans a fully traversed first-parent
//! chain in chain order and takes the first entry whose committer timestamp is
//! at or before the cutoff. Committer timestamps are not required to be
//! monotonic, so the chain is never re-sorted by time.
//!
//! Cutoffs are anchored to the series end: every cutoff is `end - k * every`
//! computed from `end` directly, so month-length clamping cannot accumulate
//! drift across steps. Both endpoints are inclusive, and the exact start is
//! appended when the cadence does not land on it.

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Days, Months, NaiveDate, TimeZone, Utc};

use crate::git::Commit;

/// Upper bound on the number of checkpoints one invocation may request.
///
/// A wide range with a narrow cadence (a century of daily checkpoints) is
/// refused outright rather than measured for hours; the request must be
/// narrowed or the cadence widened.
pub const MAX_CHECKPOINTS: usize = 4_096;

/// A requested cutoff and the commit that was current at that instant.
///
/// `commit` is `None` when the cutoff predates the repository's first commit.
/// That is a measurable, reportable fact, not a failure, and it is never
/// substituted with a later commit or a zero score.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub cutoff: DateTime<Utc>,
    pub commit: Option<Commit>,
}

/// Builds the ascending checkpoint series for one invocation.
///
/// `chain` is the first-parent chain, newest first, already fully traversed.
///
/// `since` is either a strict `YYYY-MM-DD` date, taken at `00:00:00` UTC, or a
/// positive whole duration (`30d`, `6w`, `18mo`, `2y`) subtracted from the end
/// of the series.
///
/// `until` is an optional strict `YYYY-MM-DD` date, taken at `23:59:59` UTC, so
/// the whole named day is included. Without it the series ends at the
/// committer timestamp of `chain[0]`. An `until` later than the head commit is
/// allowed and simply selects the head; no commit outside `chain` is ever
/// selected, so the requested reference stays authoritative.
///
/// `every` is a positive whole cadence using the same units. Compounds
/// (`1w3d`), fractions (`1.5d`), bare `m` and zero are rejected: `mo` means
/// months, and no unit is silently guessed. Days and weeks are exact UTC spans
/// of 24 and 168 hours; months and years are calendar spans that clamp to the
/// last valid day of the target month.
pub fn checkpoints(
    chain: &[Commit],
    since: &str,
    until: Option<&str>,
    every: &str,
) -> Result<Vec<Checkpoint>> {
    let step =
        parse_span(every).map_err(|error| anyhow!("Invalid --every value {every:?}: {error}"))?;
    let end = match until {
        Some(text) => {
            let date = parse_date(text)
                .map_err(|error| anyhow!("Invalid --until value {text:?}: {error}"))?;
            end_of_day(date)?
        }
        None => match chain.first() {
            Some(head) => head.committed_at,
            None => bail!(
                "The selected history is empty, so there is no end date; pass --until to measure \
                 explicit dates"
            ),
        },
    };
    let start = match parse_boundary(since)
        .map_err(|error| anyhow!("Invalid --since value {since:?}: {error}"))?
    {
        Boundary::Date(date) => start_of_day(date)?,
        Boundary::Span(span) => subtract(end, &span, 1)?,
    };
    if start > end {
        bail!("--since {start} is after the end of the measured range {end}");
    }

    let mut cutoffs = Vec::new();
    let mut multiple: u32 = 0;
    loop {
        let cutoff = subtract(end, &step, multiple)?;
        if cutoff < start {
            break;
        }
        cutoffs.push(cutoff);
        if cutoff == start {
            break;
        }
        if cutoffs.len() >= MAX_CHECKPOINTS {
            bail!(
                "The requested range needs more than {MAX_CHECKPOINTS} checkpoints at a cadence of \
                 {every}; narrow the range or widen --every"
            );
        }
        multiple = multiple
            .checked_add(1)
            .ok_or_else(|| anyhow!("The requested cadence overflows the calendar"))?;
    }
    if cutoffs.last() != Some(&start) {
        cutoffs.push(start);
    }
    cutoffs.reverse();

    Ok(cutoffs
        .into_iter()
        .map(|cutoff| Checkpoint {
            cutoff,
            commit: select(chain, cutoff),
        })
        .collect())
}

/// The first chain entry that existed at `cutoff`, in chain order.
fn select(chain: &[Commit], cutoff: DateTime<Utc>) -> Option<Commit> {
    chain
        .iter()
        .find(|commit| commit.committed_at <= cutoff)
        .cloned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Days,
    Weeks,
    Months,
    Years,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    count: u32,
    unit: Unit,
}

enum Boundary {
    Date(NaiveDate),
    Span(Span),
}

fn parse_boundary(text: &str) -> Result<Boundary> {
    if text.contains('-') {
        return Ok(Boundary::Date(parse_date(text)?));
    }
    match parse_span(text) {
        Ok(span) => Ok(Boundary::Span(span)),
        Err(error) => bail!("{error}, and it is not a YYYY-MM-DD date"),
    }
}

/// Parses a strict `YYYY-MM-DD` calendar date.
///
/// Strict means exactly four, two and two digits: shortened fields, extra
/// whitespace, times and timezone suffixes are rejected instead of being
/// silently reinterpreted.
fn parse_date(text: &str) -> Result<NaiveDate> {
    let bytes = text.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
    if !shaped {
        bail!("expected a YYYY-MM-DD date");
    }
    let year: i32 = text[..4].parse()?;
    let month: u32 = text[5..7].parse()?;
    let day: u32 = text[8..].parse()?;
    NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| anyhow!("{text} is not a real calendar date"))
}

/// Parses a positive whole duration such as `7d`, `2w`, `3mo` or `1y`.
fn parse_span(text: &str) -> Result<Span> {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (count, unit) = text.split_at(digits);
    if count.is_empty() {
        bail!("expected a positive whole number followed by d, w, mo or y");
    }
    let unit = match unit {
        "d" => Unit::Days,
        "w" => Unit::Weeks,
        "mo" => Unit::Months,
        "y" => Unit::Years,
        "" => bail!("missing unit; use d, w, mo or y"),
        "m" => bail!("ambiguous unit \"m\"; use \"mo\" for months"),
        other => bail!("unknown unit {other:?}; use d, w, mo or y without compounds"),
    };
    let count: u32 = count.parse().map_err(|_| anyhow!("{count} is too large"))?;
    if count == 0 {
        bail!("the value must be positive");
    }
    Ok(Span { count, unit })
}

/// Subtracts `multiple * span` from `end`, anchored at `end`.
fn subtract(end: DateTime<Utc>, span: &Span, multiple: u32) -> Result<DateTime<Utc>> {
    let overflow = || anyhow!("The requested range overflows the supported calendar");
    let total = span.count.checked_mul(multiple).ok_or_else(overflow)?;
    match span.unit {
        Unit::Days => end.checked_sub_days(Days::new(u64::from(total))),
        Unit::Weeks => {
            let days = u64::from(total).checked_mul(7).ok_or_else(overflow)?;
            end.checked_sub_days(Days::new(days))
        }
        Unit::Months => end.checked_sub_months(Months::new(total)),
        Unit::Years => {
            let months = total.checked_mul(12).ok_or_else(overflow)?;
            end.checked_sub_months(Months::new(months))
        }
    }
    .ok_or_else(overflow)
}

fn start_of_day(date: NaiveDate) -> Result<DateTime<Utc>> {
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("{date} has no start of day in UTC"))?;
    Ok(Utc.from_utc_datetime(&naive))
}

fn end_of_day(date: NaiveDate) -> Result<DateTime<Utc>> {
    let naive = date
        .and_hms_opt(23, 59, 59)
        .ok_or_else(|| anyhow!("{date} has no end of day in UTC"))?;
    Ok(Utc.from_utc_datetime(&naive))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, at: &str) -> Commit {
        Commit {
            sha: sha.to_owned(),
            committed_at: at.parse::<DateTime<Utc>>().expect("timestamp"),
        }
    }

    fn cutoffs(checkpoints: &[Checkpoint]) -> Vec<String> {
        checkpoints
            .iter()
            .map(|point| point.cutoff.to_rfc3339())
            .collect()
    }

    fn chain() -> Vec<Commit> {
        vec![
            commit(
                "cccccccccccccccccccccccccccccccccccccccc",
                "2024-03-15T10:00:00Z",
            ),
            commit(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "2024-02-10T10:00:00Z",
            ),
            commit(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "2024-01-05T10:00:00Z",
            ),
        ]
    }

    #[test]
    fn parses_strict_dates_including_leap_days() {
        assert_eq!(
            parse_date("2024-02-29").expect("leap day"),
            NaiveDate::from_ymd_opt(2024, 2, 29).expect("date")
        );
        assert!(parse_date("2023-02-29").is_err());
        assert!(parse_date("2024-1-05").is_err());
        assert!(parse_date("2024-01-05T00:00:00Z").is_err());
        assert!(parse_date(" 2024-01-05").is_err());
        assert!(parse_date("2024-13-01").is_err());
        assert!(parse_date("2024-00-10").is_err());
    }

    #[test]
    fn parses_only_whole_positive_spans() {
        assert_eq!(
            parse_span("7d").expect("days"),
            Span {
                count: 7,
                unit: Unit::Days
            }
        );
        assert_eq!(
            parse_span("2w").expect("weeks"),
            Span {
                count: 2,
                unit: Unit::Weeks
            }
        );
        assert_eq!(
            parse_span("18mo").expect("months"),
            Span {
                count: 18,
                unit: Unit::Months
            }
        );
        assert_eq!(
            parse_span("3y").expect("years"),
            Span {
                count: 3,
                unit: Unit::Years
            }
        );
        for invalid in [
            "", "d", "1", "0d", "0mo", "1m", "1.5d", "1w3d", "1 d", "-1d", "1x", "w1",
        ] {
            assert!(parse_span(invalid).is_err(), "{invalid:?} must be rejected");
        }
        assert!(parse_span("99999999999d").is_err());
    }

    #[test]
    fn anchors_month_cadence_to_the_end_without_drift() {
        let head = commit(
            "1111111111111111111111111111111111111111",
            "2024-03-31T12:00:00Z",
        );
        let points = checkpoints(&[head], "2023-12-31", None, "1mo").expect("checkpoints");
        assert_eq!(
            cutoffs(&points),
            vec![
                "2023-12-31T00:00:00+00:00",
                "2023-12-31T12:00:00+00:00",
                "2024-01-31T12:00:00+00:00",
                "2024-02-29T12:00:00+00:00",
                "2024-03-31T12:00:00+00:00",
            ]
        );
    }

    #[test]
    fn includes_both_endpoints_exactly() {
        let points =
            checkpoints(&chain(), "2024-01-01", Some("2024-03-31"), "1mo").expect("checkpoints");
        assert_eq!(
            cutoffs(&points),
            vec![
                "2024-01-01T00:00:00+00:00",
                "2024-01-31T23:59:59+00:00",
                "2024-02-29T23:59:59+00:00",
                "2024-03-31T23:59:59+00:00",
            ]
        );
        let single = checkpoints(&chain(), "2024-03-31", Some("2024-03-31"), "1d")
            .expect("single checkpoint");
        assert_eq!(single.len(), 2);
        assert_eq!(
            cutoffs(&single),
            vec!["2024-03-31T00:00:00+00:00", "2024-03-31T23:59:59+00:00"]
        );
    }

    #[test]
    fn steps_weeks_and_days_exactly() {
        let points = checkpoints(&chain(), "3w", Some("2024-03-22"), "1w").expect("checkpoints");
        assert_eq!(
            cutoffs(&points),
            vec![
                "2024-03-01T23:59:59+00:00",
                "2024-03-08T23:59:59+00:00",
                "2024-03-15T23:59:59+00:00",
                "2024-03-22T23:59:59+00:00",
            ]
        );
        let daily = checkpoints(&chain(), "2d", Some("2024-03-02"), "1d").expect("checkpoints");
        assert_eq!(daily.len(), 3);
        assert_eq!(daily[0].cutoff.to_rfc3339(), "2024-02-29T23:59:59+00:00");
    }

    #[test]
    fn appends_the_exact_start_when_the_cadence_misses_it() {
        let points =
            checkpoints(&chain(), "2024-01-10", Some("2024-03-01"), "1mo").expect("checkpoints");
        assert_eq!(
            cutoffs(&points),
            vec![
                "2024-01-10T00:00:00+00:00",
                "2024-02-01T23:59:59+00:00",
                "2024-03-01T23:59:59+00:00",
            ]
        );
    }

    #[test]
    fn defaults_the_end_to_the_head_commit() {
        let points = checkpoints(&chain(), "1mo", None, "1mo").expect("checkpoints");
        assert_eq!(
            cutoffs(&points),
            vec!["2024-02-15T10:00:00+00:00", "2024-03-15T10:00:00+00:00"]
        );
        assert_eq!(points[1].commit.as_ref().expect("head").sha, chain()[0].sha);
    }

    #[test]
    fn selects_the_first_chain_entry_at_or_before_each_cutoff() {
        let points =
            checkpoints(&chain(), "2023-12-01", Some("2024-03-31"), "1mo").expect("checkpoints");
        let selected: Vec<_> = points
            .iter()
            .map(|point| point.commit.as_ref().map(|commit| commit.sha.as_str()))
            .collect();
        assert_eq!(
            selected,
            vec![
                None,
                None,
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                Some("cccccccccccccccccccccccccccccccccccccccc"),
            ]
        );
        assert_eq!(
            cutoffs(&points),
            vec![
                "2023-12-01T00:00:00+00:00",
                "2023-12-31T23:59:59+00:00",
                "2024-01-31T23:59:59+00:00",
                "2024-02-29T23:59:59+00:00",
                "2024-03-31T23:59:59+00:00",
            ]
        );
    }

    #[test]
    fn repeats_a_commit_when_no_newer_commit_exists() {
        let points =
            checkpoints(&chain(), "2024-03-16", Some("2024-03-20"), "1d").expect("checkpoints");
        // Five daily cutoffs anchored at the end, plus the exact start.
        assert_eq!(points.len(), 6);
        assert!(
            points
                .iter()
                .all(|point| point.commit.as_ref().expect("commit").sha == chain()[0].sha)
        );
    }

    #[test]
    fn reports_prehistory_as_an_absent_commit() {
        let points =
            checkpoints(&chain(), "2020-01-01", Some("2020-06-01"), "1y").expect("checkpoints");
        assert!(points.iter().all(|point| point.commit.is_none()));
    }

    #[test]
    fn honors_chain_order_with_non_monotonic_timestamps() {
        // The middle commit records a committer time in the future; chain order,
        // not timestamp order, decides which entry is first eligible.
        let history = vec![
            commit(
                "cccccccccccccccccccccccccccccccccccccccc",
                "2024-03-15T10:00:00Z",
            ),
            commit(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "2025-01-01T00:00:00Z",
            ),
            commit(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "2024-01-05T10:00:00Z",
            ),
        ];
        let points =
            checkpoints(&history, "2024-02-01", Some("2024-02-29"), "1mo").expect("checkpoints");
        assert_eq!(
            points
                .iter()
                .map(|point| point.commit.as_ref().expect("commit").sha.as_str())
                .collect::<Vec<_>>(),
            vec![
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ]
        );
    }

    #[test]
    fn selects_the_head_when_until_is_after_it() {
        let points =
            checkpoints(&chain(), "2024-03-15", Some("2030-01-01"), "1y").expect("checkpoints");
        assert_eq!(
            points
                .last()
                .expect("last")
                .commit
                .as_ref()
                .expect("commit")
                .sha,
            chain()[0].sha
        );
    }

    #[test]
    fn rejects_inverted_ranges_and_overflow() {
        assert!(checkpoints(&chain(), "2024-05-01", Some("2024-01-01"), "1d").is_err());
        assert!(checkpoints(&chain(), "4000000y", Some("2024-01-01"), "1y").is_err());
        assert!(checkpoints(&chain(), "1000000000d", None, "1d").is_err());
        assert!(checkpoints(&[], "2024-01-01", None, "1d").is_err());
        assert!(checkpoints(&chain(), "2024-01-01", Some("2024-02-30"), "1d").is_err());
        assert!(checkpoints(&chain(), "2024-01-01", Some("2024-03-01"), "1m").is_err());
    }

    #[test]
    fn refuses_unbounded_checkpoint_counts() {
        let error = checkpoints(&chain(), "2000-01-01", Some("2024-03-01"), "1d")
            .expect_err("too many checkpoints");
        assert!(error.to_string().contains("checkpoints"), "{error}");
    }
}
