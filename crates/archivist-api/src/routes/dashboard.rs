//! Dashboard, cost budget and statistics endpoints.

use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct DashboardQuery {
    pub(crate) range: Option<String>,
}

pub(crate) async fn dashboard(
    State(state): State<AppState>,
    _auth: Authenticated,
    Query(query): Query<DashboardQuery>,
) -> ApiResult<Json<Value>> {
    let range = query
        .range
        .as_deref()
        .unwrap_or(DashboardRange::default().key())
        .parse::<DashboardRange>()
        .unwrap_or_default();
    let counts = get_backlog_counts(&state.pool).await?;
    let settings = get_runtime_settings(&state.pool).await?;
    let now = Utc::now();
    let start = dashboard_range_start(&state.pool, range, now).await?;
    let mut stats = get_dashboard_stats(&state.pool, range, &counts, now, start).await?;
    enrich_dashboard_costs(&mut stats, &settings);

    let bucket_entries = provider_bucket_entries(&state.pool, start, now, range).await?;
    let bucket_labels = dashboard_bucket_labels(start, now, range);
    enrich_provider_sparklines(&mut stats, &bucket_entries, &bucket_labels, &settings);
    let budget = dashboard_cost_budget(&state.pool, &settings, now).await?;

    Ok(Json(
        json!({ "counts": counts, "stats": stats, "budget": budget }),
    ))
}

/// Month-to-date AI cost against the configured monthly budget (#450).
/// Independent of the selected dashboard range: budgets are calendar-month
/// (UTC). `None` when no budget is configured. The cost uses the same
/// estimate as the dashboard KPIs (recorded AI usage x per-provider token
/// prices), so Document Chat answers, which are not recorded as AI
/// artifacts, are not included.
pub(crate) async fn dashboard_cost_budget(
    pool: &DbPool,
    settings: &RuntimeSettings,
    now: DateTime<Utc>,
) -> Result<Option<Value>> {
    let Some(budget_usd) = settings.ui.monthly_cost_budget_usd else {
        return Ok(None);
    };
    let month_start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(now);
    let mut usage = archivist_db::provider_usage(pool, month_start).await?;
    enrich_provider_usage_costs(&mut usage, settings);
    let month_to_date = month_to_date_cost(&usage);
    Ok(Some(cost_budget_json(
        budget_usd,
        settings.ui.cost_budget_warning_percent,
        month_to_date,
        month_start,
    )))
}

/// Reject budget values outside the documented domain instead of silently
/// clamping them on save (#450).
pub(crate) fn validate_cost_budget_settings(ui: &archivist_core::UiSettings) -> ApiResult<()> {
    if let Some(budget) = ui.monthly_cost_budget_usd
        && !(budget.is_finite() && (0.0..=1_000_000_000.0).contains(&budget))
    {
        return Err(ApiError::bad_request(
            "monthly cost budget must be between 0 and 1000000000 USD",
        ));
    }
    if !(1..=100).contains(&ui.cost_budget_warning_percent) {
        return Err(ApiError::bad_request(
            "cost budget warning percentage must be between 1 and 100",
        ));
    }
    Ok(())
}

/// Sum of the priced usage rows; `None` when no row has a price, so an
/// unpriced setup reports "unknown" instead of a misleading $0. (#450)
pub(crate) fn month_to_date_cost(usage: &[ProviderUsageStats]) -> Option<f64> {
    let priced = usage
        .iter()
        .filter_map(|item| item.estimated_cost_usd)
        .collect::<Vec<_>>();
    if priced.is_empty() {
        None
    } else {
        Some(priced.iter().sum())
    }
}

pub(crate) fn cost_budget_json(
    budget_usd: f64,
    warning_percent: u8,
    month_to_date_cost_usd: Option<f64>,
    month_start: DateTime<Utc>,
) -> Value {
    let (level, percent_used) =
        archivist_core::cost_budget_level(budget_usd, warning_percent, month_to_date_cost_usd);
    json!({
        "monthly_budget_usd": budget_usd,
        "warning_percent": warning_percent,
        "month_start": month_start,
        "month_to_date_cost_usd": month_to_date_cost_usd,
        "percent_used": percent_used,
        "level": level,
    })
}

pub(crate) async fn dashboard_live(
    State(state): State<AppState>,
    _auth: Authenticated,
) -> ApiResult<Json<Value>> {
    let settings = get_runtime_settings(&state.pool).await?;
    Ok(Json(json!(
        get_dashboard_live_status(&state.pool, &settings).await?
    )))
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatisticsQuery {
    /// RFC3339 / `YYYY-MM-DD` start (inclusive). Defaults to `to - 30 days`.
    pub(crate) from: Option<String>,
    /// RFC3339 / `YYYY-MM-DD` end (exclusive). A bare date means the END of
    /// that day (next UTC midnight), so the named day is fully covered (#301).
    /// Defaults to now.
    pub(crate) to: Option<String>,
    /// Bucket granularity: hour | day | week | month. Defaults to day.
    pub(crate) bucket: Option<String>,
}

/// How a bare `YYYY-MM-DD` statistics bound anchors within its day. RFC3339
/// inputs carry their own time and are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatBound {
    /// Inclusive range start: the named day's first instant (00:00:00 UTC).
    Start,
    /// Exclusive range end: the NEXT day's first instant, so the named day is
    /// fully covered. Anchoring `to` at its own midnight made the current day
    /// invisible (`to=<today>` excluded everything after 00:00) and turned
    /// `from == to` into an empty range. #301
    End,
}

pub(crate) fn parse_stat_datetime(raw: &str, bound: StatBound) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    // Accept a bare date (YYYY-MM-DD) interpreted as UTC midnight.
    let mut date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    if bound == StatBound::End {
        date = date.succ_opt()?;
    }
    Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?))
}

/// Resolve the statistics range from the raw query parameters. Defaults:
/// `to` = `now`, `from` = `to` - 30 days — so the default view always ends at
/// the current instant and includes today's data.
///
/// Defaults apply only when a bound is absent (or blank); a value that is
/// present but unparseable is rejected with 400 instead of being silently
/// swapped for the default, which hid typos behind a wrong-looking range. #312
pub(crate) fn resolve_stat_range(
    from: Option<&str>,
    to: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
    let to = match to.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => now,
        Some(raw) => parse_stat_datetime(raw, StatBound::End).ok_or_else(|| {
            ApiError::bad_request("'to' must be an RFC3339 timestamp or a YYYY-MM-DD date")
        })?,
    };
    let from = match from.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => to - Duration::days(30),
        Some(raw) => parse_stat_datetime(raw, StatBound::Start).ok_or_else(|| {
            ApiError::bad_request("'from' must be an RFC3339 timestamp or a YYYY-MM-DD date")
        })?,
    };
    if from >= to {
        return Err(ApiError::bad_request("'from' must be before 'to'"));
    }
    Ok((from, to))
}

/// Hard ceiling for the zero-filled statistics axis (#312). Covers 90 days of
/// hour buckets (2160) with headroom and ~6.8 years of day buckets; past the
/// cap (e.g. hour buckets over a multi-year span) the series simply stay
/// sparse.
pub(crate) const MAX_STATISTICS_BUCKETS: usize = 2500;

/// Floor `ts` to the statistics bucket unit, mirroring Postgres
/// `date_trunc(unit, ts)` under a UTC session: weeks start on the ISO Monday,
/// months on the 1st.
pub(crate) fn statistics_bucket_floor(ts: DateTime<Utc>, bucket: &str) -> DateTime<Utc> {
    let date = ts.date_naive();
    let (date, hour) = match bucket {
        "hour" => (date, ts.hour()),
        "week" => (
            date - Duration::days(i64::from(date.weekday().num_days_from_monday())),
            0,
        ),
        "month" => (date.with_day(1).unwrap_or(date), 0),
        // "day" (the only other validated unit)
        _ => (date, 0),
    };
    date.and_hms_opt(hour, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))
        .unwrap_or(ts)
}

/// The start of the bucket after `cursor`: hour/day/week step by a fixed
/// span, month advances to the 1st of the next month (mirroring
/// `dashboard_bucket_labels`).
pub(crate) fn statistics_bucket_next(cursor: DateTime<Utc>, bucket: &str) -> Option<DateTime<Utc>> {
    match bucket {
        "hour" => Some(cursor + Duration::hours(1)),
        "week" => Some(cursor + Duration::days(7)),
        "month" => {
            let (year, month) = if cursor.month() == 12 {
                (cursor.year() + 1, 1)
            } else {
                (cursor.year(), cursor.month() + 1)
            };
            Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).single()
        }
        _ => Some(cursor + Duration::days(1)),
    }
}

/// Every bucket start covering `[from, to)`, used to zero-fill the statistics
/// time series so quiet periods chart as 0 instead of being skipped (the SQL
/// GROUP BY only yields non-empty buckets). #312
///
/// The axis never extends before the first bucket that actually holds data:
/// the "all time" preset sends a far-past sentinel `from`, and mirroring the
/// dashboard — whose "all" range starts at the earliest record — keeps that
/// meaning "the recorded span", not decades of empty buckets. Without any
/// data the requested range itself is enumerated, so the default view still
/// renders a flat zero axis. Spans past `MAX_STATISTICS_BUCKETS` return an
/// empty list and the series simply stay sparse.
pub(crate) fn statistics_bucket_starts(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket: &str,
    earliest_data: Option<DateTime<Utc>>,
) -> Vec<DateTime<Utc>> {
    let mut cursor = statistics_bucket_floor(from, bucket);
    if let Some(earliest) = earliest_data {
        cursor = cursor.max(earliest);
    }
    let mut starts = Vec::new();
    while cursor < to {
        if starts.len() >= MAX_STATISTICS_BUCKETS {
            return Vec::new();
        }
        starts.push(cursor);
        match statistics_bucket_next(cursor, bucket) {
            Some(next) if next > cursor => cursor = next,
            _ => break,
        }
    }
    starts
}

#[derive(Default, Clone)]
pub(crate) struct UsageAgg {
    pub(crate) request_count: i64,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) duration_sum: f64,
    pub(crate) duration_n: f64,
}

impl UsageAgg {
    pub(crate) fn add(&mut self, row: &archivist_db::StatisticsUsageRow) {
        self.request_count += row.request_count;
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
        if let Some(avg) = row.avg_duration_ms {
            // Weight the per-cell average by its request count to recover a
            // correct overall mean.
            self.duration_sum += avg * row.request_count as f64;
            self.duration_n += row.request_count as f64;
        }
    }
    pub(crate) fn avg_ms(&self) -> Option<f64> {
        (self.duration_n > 0.0).then(|| self.duration_sum / self.duration_n)
    }
    pub(crate) fn cost(&self, costs: Option<&(Option<f64>, Option<f64>)>) -> Option<f64> {
        let (Some(ci), Some(co)) = *costs? else {
            return None;
        };
        Some(
            (self.input_tokens as f64 / 1_000_000.0 * ci)
                + (self.output_tokens as f64 / 1_000_000.0 * co),
        )
    }
    pub(crate) fn to_json(&self, key_field: &str, key: &str, cost: Option<f64>) -> Value {
        json!({
            key_field: key,
            "request_count": self.request_count,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "avg_duration_ms": self.avg_ms(),
            "estimated_cost_usd": cost,
        })
    }
}

/// Comprehensive Statistics page data: summary + time-series + per-provider /
/// per-model / per-stage breakdowns + pipeline throughput, over a custom range.
pub(crate) async fn statistics(
    State(state): State<AppState>,
    _auth: Authenticated,
    Query(query): Query<StatisticsQuery>,
) -> ApiResult<Json<Value>> {
    let now = Utc::now();
    let (from, to) = resolve_stat_range(query.from.as_deref(), query.to.as_deref(), now)?;
    let bucket = match query.bucket.as_deref().unwrap_or("day") {
        b @ ("hour" | "day" | "week" | "month") => b.to_owned(),
        _ => {
            return Err(ApiError::bad_request(
                "bucket must be hour, day, week or month",
            ));
        }
    };

    let usage = statistics_usage_rows(&state.pool, from, to, &bucket).await?;
    let throughput = statistics_throughput_rows(&state.pool, from, to, &bucket).await?;
    let settings = get_runtime_settings(&state.pool).await?;

    // provider name -> (input cost / 1M, output cost / 1M)
    let cost_map: HashMap<String, (Option<f64>, Option<f64>)> = settings
        .ai
        .providers
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                (
                    p.cost_per_1m_input_tokens_usd,
                    p.cost_per_1m_output_tokens_usd,
                ),
            )
        })
        .collect();

    let mut total = UsageAgg::default();
    let mut by_provider: std::collections::BTreeMap<String, UsageAgg> = Default::default();
    let mut by_model: std::collections::BTreeMap<String, UsageAgg> = Default::default();
    let mut by_stage: std::collections::BTreeMap<String, UsageAgg> = Default::default();
    let mut series: std::collections::BTreeMap<DateTime<Utc>, UsageAgg> = Default::default();

    for row in &usage {
        total.add(row);
        by_provider
            .entry(row.provider.clone())
            .or_default()
            .add(row);
        by_model.entry(row.model.clone()).or_default().add(row);
        by_stage.entry(row.stage.clone()).or_default().add(row);
        series.entry(row.bucket).or_default().add(row);
    }

    // Pipeline throughput per bucket: succeeded / failed / cancelled.
    let mut throughput_series: std::collections::BTreeMap<DateTime<Utc>, (i64, i64, i64)> =
        Default::default();
    let (mut tot_ok, mut tot_fail, mut tot_cancel) = (0_i64, 0_i64, 0_i64);
    for row in &throughput {
        let entry = throughput_series.entry(row.bucket).or_default();
        match row.status.as_str() {
            "succeeded" => {
                entry.0 += row.job_count;
                tot_ok += row.job_count;
            }
            "failed" => {
                entry.1 += row.job_count;
                tot_fail += row.job_count;
            }
            _ => {
                entry.2 += row.job_count;
                tot_cancel += row.job_count;
            }
        }
    }

    // Zero-fill the shared bucket axis so both charts plot every bucket of
    // the range and quiet periods show as 0 instead of being compressed
    // away. #312
    let earliest_data = series.keys().chain(throughput_series.keys()).min().copied();
    for bucket_start in statistics_bucket_starts(from, to, &bucket, earliest_data) {
        series.entry(bucket_start).or_default();
        throughput_series.entry(bucket_start).or_default();
    }

    let total_cost: Option<f64> = {
        let mut any = false;
        let mut sum = 0.0;
        for (name, agg) in &by_provider {
            if let Some(c) = agg.cost(cost_map.get(name)) {
                any = true;
                sum += c;
            }
        }
        any.then_some(sum)
    };

    let to_series = |s: &std::collections::BTreeMap<DateTime<Utc>, UsageAgg>| -> Vec<Value> {
        s.iter()
            .map(|(bucket, agg)| {
                json!({
                    "bucket": bucket.to_rfc3339(),
                    "request_count": agg.request_count,
                    "input_tokens": agg.input_tokens,
                    "output_tokens": agg.output_tokens,
                    "avg_duration_ms": agg.avg_ms(),
                })
            })
            .collect()
    };

    Ok(Json(json!({
        "from": from.to_rfc3339(),
        "to": to.to_rfc3339(),
        "bucket": bucket,
        "summary": {
            "request_count": total.request_count,
            "input_tokens": total.input_tokens,
            "output_tokens": total.output_tokens,
            "avg_duration_ms": total.avg_ms(),
            "estimated_cost_usd": total_cost,
            "jobs_succeeded": tot_ok,
            "jobs_failed": tot_fail,
            "jobs_cancelled": tot_cancel,
        },
        "time_series": to_series(&series),
        "throughput_series": throughput_series.iter().map(|(bucket, (ok, fail, cancel))| json!({
            "bucket": bucket.to_rfc3339(),
            "succeeded": ok,
            "failed": fail,
            "cancelled": cancel,
        })).collect::<Vec<_>>(),
        "by_provider": by_provider.iter().map(|(name, agg)| {
            agg.to_json("provider", name, agg.cost(cost_map.get(name)))
        }).collect::<Vec<_>>(),
        "by_model": by_model.iter().map(|(name, agg)| agg.to_json("model", name, None)).collect::<Vec<_>>(),
        "by_stage": by_stage.iter().map(|(name, agg)| agg.to_json("stage", name, None)).collect::<Vec<_>>(),
    })))
}

pub(crate) fn enrich_provider_usage_costs(
    usage: &mut [ProviderUsageStats],
    settings: &RuntimeSettings,
) {
    for item in usage {
        let Some(provider) = settings
            .ai
            .providers
            .iter()
            .find(|provider| provider.name == item.provider)
        else {
            continue;
        };
        let input_cost = provider.cost_per_1m_input_tokens_usd;
        let output_cost = provider.cost_per_1m_output_tokens_usd;
        item.estimated_cost_usd = match (input_cost, output_cost) {
            (Some(input), Some(output)) => Some(
                (item.input_tokens as f64 / 1_000_000.0 * input)
                    + (item.output_tokens as f64 / 1_000_000.0 * output),
            ),
            _ => None,
        };
    }
}

pub(crate) fn enrich_dashboard_costs(stats: &mut DashboardStats, settings: &RuntimeSettings) {
    enrich_provider_usage_costs(&mut stats.provider_usage, settings);

    let total_cost: f64 = stats
        .provider_usage
        .iter()
        .filter_map(|item| item.estimated_cost_usd)
        .sum();
    stats.kpis.cost_in_range_usd = if stats
        .provider_usage
        .iter()
        .any(|p| p.estimated_cost_usd.is_some())
    {
        Some(total_cost)
    } else {
        None
    };

    let mut weighted_input_cost = 0.0_f64;
    let mut weighted_input_tokens = 0_i64;
    let mut weighted_output_cost = 0.0_f64;
    let mut weighted_output_tokens = 0_i64;
    for item in &stats.provider_usage {
        let Some(provider) = settings
            .ai
            .providers
            .iter()
            .find(|provider| provider.name == item.provider)
        else {
            continue;
        };
        if let Some(rate) = provider.cost_per_1m_input_tokens_usd {
            weighted_input_cost += item.input_tokens as f64 / 1_000_000.0 * rate;
            weighted_input_tokens += item.input_tokens;
        }
        if let Some(rate) = provider.cost_per_1m_output_tokens_usd {
            weighted_output_cost += item.output_tokens as f64 / 1_000_000.0 * rate;
            weighted_output_tokens += item.output_tokens;
        }
    }
    let input_rate_per_token = if weighted_input_tokens > 0 {
        weighted_input_cost / weighted_input_tokens as f64
    } else {
        0.0
    };
    let output_rate_per_token = if weighted_output_tokens > 0 {
        weighted_output_cost / weighted_output_tokens as f64
    } else {
        0.0
    };
    let cost_known = weighted_input_tokens > 0 || weighted_output_tokens > 0;
    for bucket in &mut stats.cost_series {
        if !cost_known {
            bucket.cost_usd = None;
            continue;
        }
        if bucket.input_tokens + bucket.output_tokens == 0 {
            bucket.cost_usd = Some(0.0);
            continue;
        }
        bucket.cost_usd = Some(
            bucket.input_tokens as f64 * input_rate_per_token
                + bucket.output_tokens as f64 * output_rate_per_token,
        );
    }

    stats.cost_breakdown_by_provider = stats
        .provider_usage
        .iter()
        .map(|item| DashboardProviderCostSummary {
            provider: item.provider.clone(),
            model: item.model.clone(),
            cost_usd: item.estimated_cost_usd,
            request_count: item.request_count,
            input_tokens: item.input_tokens,
            output_tokens: item.output_tokens,
            sparkline: Vec::new(),
        })
        .collect();
}

pub(crate) fn enrich_provider_sparklines(
    stats: &mut archivist_core::DashboardStats,
    entries: &[ProviderBucketEntry],
    labels: &[(DateTime<Utc>, String)],
    settings: &RuntimeSettings,
) {
    let bucket_count = labels.len();
    if bucket_count == 0 {
        return;
    }
    // Build the bucket -> index map once; the hot loops below iterate `entries`
    // many times and previously rescanned `labels` linearly for every entry.
    let bucket_index: HashMap<DateTime<Utc>, usize> = labels
        .iter()
        .enumerate()
        .map(|(idx, (bucket, _))| (*bucket, idx))
        .collect();
    let bucket_index_of =
        |bucket: DateTime<Utc>| -> Option<usize> { bucket_index.get(&bucket).copied() };
    let rate_for = |provider_name: &str| -> Option<(f64, f64)> {
        let provider = settings
            .ai
            .providers
            .iter()
            .find(|p| p.name == provider_name)?;
        match (
            provider.cost_per_1m_input_tokens_usd,
            provider.cost_per_1m_output_tokens_usd,
        ) {
            (Some(input), Some(output)) => Some((input, output)),
            _ => None,
        }
    };

    for summary in stats.cost_breakdown_by_provider.iter_mut() {
        let mut buckets: Vec<Option<f64>> = vec![None; bucket_count];
        let rate = rate_for(&summary.provider);
        for entry in entries
            .iter()
            .filter(|e| e.provider == summary.provider && e.model == summary.model)
        {
            let Some(idx) = bucket_index_of(entry.bucket) else {
                continue;
            };
            if let Some((input_rate, output_rate)) = rate {
                let cost = entry.input_tokens as f64 / 1_000_000.0 * input_rate
                    + entry.output_tokens as f64 / 1_000_000.0 * output_rate;
                let slot = &mut buckets[idx];
                *slot = Some(slot.unwrap_or(0.0) + cost);
            }
        }
        summary.sparkline = buckets;
    }

    for usage in stats.provider_usage.iter_mut() {
        let mut buckets: Vec<Option<f64>> = vec![None; bucket_count];
        for entry in entries.iter().filter(|e| {
            e.provider == usage.provider && e.model == usage.model && e.stage == usage.stage
        }) {
            let Some(idx) = bucket_index_of(entry.bucket) else {
                continue;
            };
            buckets[idx] = entry.avg_duration_ms;
        }
        usage.latency_history = buckets;
    }
}
