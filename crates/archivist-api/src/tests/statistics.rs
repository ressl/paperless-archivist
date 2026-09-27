//! Statistics range/bucket and inventory date filter tests.

use crate::*;

fn utc(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
}

#[test]
fn stat_bare_date_to_covers_the_whole_day() {
    // #301: a bare `to` date is the EXCLUSIVE end of that day, a bare
    // `from` its first instant. RFC3339 inputs keep their own time.
    assert_eq!(
        parse_stat_datetime("2026-06-11", StatBound::Start),
        Some(utc(2026, 6, 11, 0, 0, 0))
    );
    assert_eq!(
        parse_stat_datetime("2026-06-11", StatBound::End),
        Some(utc(2026, 6, 12, 0, 0, 0))
    );
    assert_eq!(
        parse_stat_datetime("2026-06-11T08:30:00Z", StatBound::End),
        Some(utc(2026, 6, 11, 8, 30, 0))
    );
    assert_eq!(parse_stat_datetime("not-a-date", StatBound::End), None);
}

#[test]
fn inventory_date_filters_parse_or_reject() {
    // #315: absent/blank means "no filter"; present-but-garbage is a 400
    // (same contract as the statistics range, #312) instead of silently
    // matching nothing against the typed date column.
    assert_eq!(
        parse_inventory_date_filter("date_from", None).unwrap(),
        None
    );
    assert_eq!(
        parse_inventory_date_filter("date_from", Some("  ")).unwrap(),
        None
    );
    assert_eq!(
        parse_inventory_date_filter("date_from", Some("2026-06-11")).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 6, 11)
    );
    assert!(parse_inventory_date_filter("date_to", Some("11.06.2026")).is_err());
    assert!(parse_inventory_date_filter("date_to", Some("2026-13-40")).is_err());
}

#[test]
fn statistics_default_view_includes_today() {
    // Default view (no from/to): `to` is "now", so data recorded earlier
    // today is inside the half-open [from, to) window. #301
    let now = utc(2026, 6, 11, 15, 45, 0);
    let (from, to) = resolve_stat_range(None, None, now).expect("default range");
    assert_eq!(to, now);
    assert_eq!(from, now - Duration::days(30));
    let earlier_today = utc(2026, 6, 11, 0, 5, 0);
    assert!(from <= earlier_today && earlier_today < to);

    // The UI used to send `to=<today>` as a bare date; that must also
    // cover the whole current day instead of cutting off at midnight.
    let (_, to) = resolve_stat_range(None, Some("2026-06-11"), now).expect("bare to");
    assert_eq!(to, utc(2026, 6, 12, 0, 0, 0));
    assert!(now < to);
}

#[test]
fn statistics_single_day_range_is_valid() {
    // #301: `from == to` on a bare date means "exactly that day", not an
    // empty range rejected with 400.
    let now = utc(2026, 6, 11, 15, 45, 0);
    let (from, to) =
        resolve_stat_range(Some("2026-06-10"), Some("2026-06-10"), now).expect("single-day range");
    assert_eq!(from, utc(2026, 6, 10, 0, 0, 0));
    assert_eq!(to, utc(2026, 6, 11, 0, 0, 0));

    // Inverted bounds are still rejected.
    assert!(resolve_stat_range(Some("2026-06-11"), Some("2026-06-10"), now).is_err());
}

#[test]
fn statistics_unparseable_bounds_are_rejected() {
    // #312: defaults only apply to ABSENT bounds; garbage is a 400, not a
    // silent fallback to the default range.
    let now = utc(2026, 6, 11, 15, 45, 0);
    assert!(resolve_stat_range(Some("not-a-date"), None, now).is_err());
    assert!(resolve_stat_range(None, Some("2026-13-77"), now).is_err());

    // Blank values count as absent, not as garbage.
    let (from, to) = resolve_stat_range(Some(" "), Some(""), now).expect("blank = defaults");
    assert_eq!(to, now);
    assert_eq!(from, now - Duration::days(30));
}

#[test]
fn statistics_bucket_floor_mirrors_date_trunc() {
    let ts = utc(2026, 6, 11, 15, 45, 7); // a Thursday
    assert_eq!(
        statistics_bucket_floor(ts, "hour"),
        utc(2026, 6, 11, 15, 0, 0)
    );
    assert_eq!(
        statistics_bucket_floor(ts, "day"),
        utc(2026, 6, 11, 0, 0, 0)
    );
    // date_trunc('week') floors to the ISO Monday.
    assert_eq!(
        statistics_bucket_floor(ts, "week"),
        utc(2026, 6, 8, 0, 0, 0)
    );
    assert_eq!(
        statistics_bucket_floor(ts, "month"),
        utc(2026, 6, 1, 0, 0, 0)
    );
}

#[test]
fn statistics_bucket_next_steps_each_granularity() {
    let monday = utc(2026, 6, 8, 0, 0, 0);
    assert_eq!(
        statistics_bucket_next(monday, "hour"),
        Some(utc(2026, 6, 8, 1, 0, 0))
    );
    assert_eq!(
        statistics_bucket_next(monday, "day"),
        Some(utc(2026, 6, 9, 0, 0, 0))
    );
    assert_eq!(
        statistics_bucket_next(monday, "week"),
        Some(utc(2026, 6, 15, 0, 0, 0))
    );
    assert_eq!(
        statistics_bucket_next(utc(2026, 6, 1, 0, 0, 0), "month"),
        Some(utc(2026, 7, 1, 0, 0, 0))
    );
    // Month rollover across the year boundary.
    assert_eq!(
        statistics_bucket_next(utc(2026, 12, 1, 0, 0, 0), "month"),
        Some(utc(2027, 1, 1, 0, 0, 0))
    );
}

#[test]
fn statistics_zero_fill_enumerates_the_requested_range() {
    // #312: every bucket of [from, to) appears, including empty interior /
    // trailing ones, mirroring dashboard_bucket_labels. With no data at
    // all the requested range itself is enumerated (flat zero axis).
    let from = utc(2026, 6, 9, 12, 0, 0);
    let to = utc(2026, 6, 11, 15, 45, 0);
    assert_eq!(
        statistics_bucket_starts(from, to, "day", None),
        vec![
            utc(2026, 6, 9, 0, 0, 0),
            utc(2026, 6, 10, 0, 0, 0),
            utc(2026, 6, 11, 0, 0, 0),
        ]
    );
    // The axis never starts before the first bucket holding data.
    assert_eq!(
        statistics_bucket_starts(from, to, "day", Some(utc(2026, 6, 10, 0, 0, 0))),
        vec![utc(2026, 6, 10, 0, 0, 0), utc(2026, 6, 11, 0, 0, 0)]
    );
}

#[test]
fn statistics_zero_fill_clamps_all_time_to_earliest_data() {
    // "all time" (far-past from): the axis starts at the earliest bucket
    // that actually has data, mirroring the dashboard's "all" range...
    let from = utc(2000, 1, 1, 0, 0, 0);
    let to = utc(2026, 6, 11, 15, 0, 0);
    let earliest = Some(utc(2026, 6, 9, 0, 0, 0));
    assert_eq!(
        statistics_bucket_starts(from, to, "day", earliest),
        vec![
            utc(2026, 6, 9, 0, 0, 0),
            utc(2026, 6, 10, 0, 0, 0),
            utc(2026, 6, 11, 0, 0, 0),
        ]
    );
    // ...stays sparse without any data (the sentinel span blows the cap)...
    assert!(statistics_bucket_starts(from, to, "day", None).is_empty());
    // ...and stays sparse when even the data span exceeds the cap.
    let ancient = Some(utc(2000, 2, 7, 0, 0, 0));
    assert!(statistics_bucket_starts(from, to, "hour", ancient).is_empty());
}
