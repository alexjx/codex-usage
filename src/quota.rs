//! Quota snapshots and deliberately conservative, recent-workload forecasts.
//!
//! Quota percentages belong to an account, not to a log file. Source identities
//! are kept separate because a rollout does not identify the signed-in account.
//! Estimates describe the locally observed workload, never a token entitlement.
use crate::model::UsageEvent;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_WINDOW_MINUTES: u64 = 7 * 24 * 60;
const MAX_SNAPSHOT_AGE_SECONDS: i64 = 5 * 60;
const FORECAST_WINDOW_SECONDS: i64 = 60 * 60;
const MAX_SAMPLE_GAP_SECONDS: i64 = 10 * 60;
const RESET_JITTER_SECONDS: i64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaSnapshot {
    pub timestamp: DateTime<Utc>,
    pub source: String,
    pub limit_id: String,
    pub bucket: String,
    pub used_percent: f64,
    pub window_minutes: u64,
    pub resets_at: DateTime<Utc>,
}

impl QuotaSnapshot {
    /// Parse only quota fields; credits/balances and conversation text are unused.
    pub fn parse(timestamp: DateTime<Utc>, limits: &Value, source: &str) -> Vec<Self> {
        ["primary", "secondary"]
            .into_iter()
            .filter_map(|bucket| {
                let limit = limits.get(bucket)?;
                let snapshot = Self {
                    timestamp,
                    source: source.into(),
                    limit_id: limits["limit_id"].as_str().unwrap_or("codex").into(),
                    bucket: bucket.into(),
                    used_percent: limit["used_percent"].as_f64()?,
                    window_minutes: limit["window_minutes"].as_u64()?,
                    resets_at: DateTime::from_timestamp(limit["resets_at"].as_i64()?, 0)?,
                };
                snapshot.valid().then_some(snapshot)
            })
            .collect()
    }

    fn valid(&self) -> bool {
        self.used_percent.is_finite()
            && (0.0..=100.0).contains(&self.used_percent)
            && (1..=MAX_WINDOW_MINUTES).contains(&self.window_minutes)
            && self.resets_at > self.timestamp
            && (self.resets_at - self.timestamp).num_seconds()
                <= self.window_minutes as i64 * 60 + 300
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaSummary {
    pub source: String,
    pub limit_id: String,
    pub bucket: String,
    pub used_percent: f64,
    pub window_minutes: u64,
    pub observed_at: DateTime<Utc>,
    pub next_reset: DateTime<Utc>,
    pub last_reset: Option<DateTime<Utc>>,
    /// `observed` means an earlier snapshot recorded this reset deadline.
    pub last_reset_basis: String,
    /// `current`, `stale`, or `expired` describes the last recorded snapshot.
    pub status: String,
    /// Approximate additional input+output tokens at the recent workload mix.
    /// This is a snapshot-time estimate, not a subscription token allowance.
    pub remaining_tokens: Option<u64>,
    pub exhausts_at: Option<DateTime<Utc>>,
    pub forecast_status: String,
    pub forecast_window_seconds: Option<f64>,
    pub forecast_used_percent: Option<f64>,
    pub forecast_tokens: Option<u64>,
}

/// Summarize only data the caller already loaded. No history discovery is done.
pub fn summarize(
    snapshots: &[QuotaSnapshot],
    events: &[UsageEvent],
    now: DateTime<Utc>,
    incomplete: bool,
) -> Vec<QuotaSummary> {
    let mut groups: BTreeMap<(&str, &str, &str), Vec<&QuotaSnapshot>> = BTreeMap::new();
    for snapshot in snapshots {
        // Do not let future-dated logs replace the most recent usable snapshot.
        if snapshot.valid() && snapshot.timestamp <= now {
            groups
                .entry((&snapshot.source, &snapshot.limit_id, &snapshot.bucket))
                .or_default()
                .push(snapshot);
        }
    }
    groups
        .into_values()
        .map(|mut samples| {
            samples.sort_by(|a, b| {
                a.timestamp
                    .cmp(&b.timestamp)
                    .then(a.resets_at.cmp(&b.resets_at))
                    .then(a.used_percent.total_cmp(&b.used_percent))
            });
            samples.dedup();
            let last = *samples.last().unwrap();
            let inferred = last
                .resets_at
                .checked_sub_signed(Duration::minutes(last.window_minutes as i64))
                .filter(|reset| *reset <= last.timestamp);
            let recorded = inferred.and_then(|inferred| {
                samples
                    .iter()
                    .filter(|s| {
                        s.window_minutes == last.window_minutes
                            && s.timestamp < last.timestamp
                            && s.resets_at <= last.timestamp
                            && (s.resets_at - inferred).num_seconds().abs() <= 300
                            && (last.resets_at - s.resets_at).num_seconds() > RESET_JITTER_SECONDS
                    })
                    .map(|s| s.resets_at)
                    .max()
            });
            let last_reset = recorded.or(inferred);
            let status = if last.resets_at <= now {
                "expired"
            } else if (now - last.timestamp).num_seconds() > MAX_SNAPSHOT_AGE_SECONDS {
                "stale"
            } else {
                "current"
            };
            let mut summary = QuotaSummary {
                source: last.source.clone(),
                limit_id: last.limit_id.clone(),
                bucket: last.bucket.clone(),
                used_percent: last.used_percent,
                window_minutes: last.window_minutes,
                observed_at: last.timestamp,
                next_reset: last.resets_at,
                last_reset,
                last_reset_basis: if recorded.is_some() {
                    "observed"
                } else if inferred.is_some() {
                    "estimated"
                } else {
                    "unavailable"
                }
                .into(),
                status: status.into(),
                remaining_tokens: None,
                exhausts_at: None,
                forecast_status: "insufficient_history".into(),
                forecast_window_seconds: None,
                forecast_used_percent: None,
                forecast_tokens: None,
            };
            forecast(&mut summary, &samples, events, now, incomplete);
            summary
        })
        .collect()
}

fn forecast(
    summary: &mut QuotaSummary,
    samples: &[&QuotaSnapshot],
    events: &[UsageEvent],
    now: DateTime<Utc>,
    incomplete: bool,
) {
    let unavailable = if summary.status == "expired" {
        Some("expired")
    } else if incomplete {
        Some("incomplete_data")
    } else if summary.status == "stale" {
        Some("stale")
    } else {
        None
    };
    if let Some(reason) = unavailable {
        summary.forecast_status = reason.into();
        return;
    }
    if samples.iter().any(|sample| {
        sample.timestamp == summary.observed_at
            && (sample.used_percent != summary.used_percent
                || sample.window_minutes != summary.window_minutes
                || (sample.resets_at - summary.next_reset).num_seconds().abs()
                    > RESET_JITTER_SECONDS)
    }) {
        summary.forecast_status = "conflicting_snapshots".into();
        return;
    }
    if summary.used_percent == 100.0 {
        summary.remaining_tokens = Some(0);
        summary.forecast_status = "exhausted".into();
        return;
    }
    let start = summary.observed_at - Duration::seconds(FORECAST_WINDOW_SECONDS);
    let samples: Vec<_> = samples
        .iter()
        .copied()
        .filter(|s| {
            s.timestamp >= start
                && s.window_minutes == summary.window_minutes
                && (s.resets_at - summary.next_reset).num_seconds().abs() <= RESET_JITTER_SECONDS
                && summary.last_reset.is_none_or(|reset| s.timestamp >= reset)
        })
        .collect();
    let Some(first) = samples.first() else {
        return;
    };
    if samples.windows(2).any(|pair| {
        pair[0].timestamp == pair[1].timestamp
            && (pair[0].used_percent != pair[1].used_percent
                || (pair[0].resets_at - pair[1].resets_at).num_seconds().abs()
                    > RESET_JITTER_SECONDS)
    }) {
        summary.forecast_status = "conflicting_snapshots".into();
        return;
    }
    if samples
        .windows(2)
        .any(|pair| pair[1].used_percent < pair[0].used_percent)
    {
        summary.forecast_status = "quota_changed".into();
        return;
    }
    if samples
        .windows(2)
        .any(|pair| (pair[1].timestamp - pair[0].timestamp).num_seconds() > MAX_SAMPLE_GAP_SECONDS)
    {
        summary.forecast_status = "sparse_history".into();
        return;
    }
    let seconds = (summary.observed_at - first.timestamp).num_milliseconds() as f64 / 1000.0;
    let consumed = summary.used_percent - first.used_percent;
    if seconds < 120.0 || consumed < 2.0 {
        return;
    }
    summary.forecast_window_seconds = Some(seconds);
    summary.forecast_used_percent = Some(consumed);
    let mut unique = BTreeMap::<&str, &UsageEvent>::new();
    for event in events.iter().filter(|event| {
        event.timestamp > first.timestamp
            && event.timestamp <= summary.observed_at
            && (event.source == summary.source || event.origins.contains(&summary.source))
            && event.provider == "openai"
    }) {
        if let Some(previous) = unique.insert(&event.id, event) {
            if previous != event {
                summary.forecast_status = "inconsistent_usage".into();
                return;
            }
        }
    }
    if unique.is_empty() {
        summary.forecast_status = "missing_usage".into();
        return;
    }
    // A large shift between the first and second halves means quota/token
    // conversion has changed. Wait for a stable workload rather than project it.
    if !stable_workload(
        unique.values().copied(),
        first.timestamp,
        summary.observed_at,
    ) {
        summary.forecast_status = "changing_workload".into();
        return;
    }
    let Some(tokens) = unique
        .values()
        .try_fold(0u64, |total, event| total.checked_add(event.usage.total()))
    else {
        summary.forecast_status = "usage_overflow".into();
        return;
    };
    if tokens == 0 {
        summary.forecast_status = "missing_usage".into();
        return;
    }
    let remaining = 100.0 - summary.used_percent;
    let estimated_tokens = tokens as f64 / consumed * remaining;
    let estimated_seconds = seconds / consumed * remaining;
    if !estimated_tokens.is_finite()
        || estimated_tokens >= u64::MAX as f64
        || !estimated_seconds.is_finite()
        || estimated_seconds > i64::MAX as f64 / 1000.0
    {
        summary.forecast_status = "estimate_out_of_range".into();
        return;
    }
    let Some(exhausts_at) = summary
        .observed_at
        .checked_add_signed(Duration::milliseconds((estimated_seconds * 1000.0) as i64))
    else {
        summary.forecast_status = "estimate_out_of_range".into();
        return;
    };
    if exhausts_at <= now {
        summary.forecast_status = "awaiting_update".into();
        return;
    }
    summary.forecast_tokens = Some(tokens);
    summary.remaining_tokens = Some(estimated_tokens.floor() as u64);
    if exhausts_at >= summary.next_reset {
        summary.forecast_status = "resets_first".into();
    } else {
        summary.exhausts_at = Some(exhausts_at);
        summary.forecast_status = "estimated".into();
    }
}

fn stable_workload<'a>(
    events: impl Iterator<Item = &'a UsageEvent>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> bool {
    let midpoint = start + (end - start) / 2;
    let mut halves = [BTreeMap::<(&str, &str), f64>::new(), BTreeMap::new()];
    for event in events {
        *halves[usize::from(event.timestamp > midpoint)]
            .entry((&event.model, event.mode()))
            .or_default() += event.usage.total() as f64;
    }
    let totals = halves.each_ref().map(|half| half.values().sum::<f64>());
    if totals.contains(&0.0) {
        return false;
    }
    let keys: BTreeSet<_> = halves
        .iter()
        .flat_map(|half| half.keys().copied())
        .collect();
    keys.into_iter().all(|key| {
        let first = halves[0].get(&key).copied().unwrap_or(0.0) / totals[0];
        let second = halves[1].get(&key).copied().unwrap_or(0.0) / totals[1];
        (first - second).abs() <= 0.25
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Usage;
    use serde_json::json;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }
    fn sample(seconds: i64, percent: f64) -> QuotaSnapshot {
        QuotaSnapshot {
            timestamp: time(seconds),
            source: "wsl".into(),
            limit_id: "codex".into(),
            bucket: "primary".into(),
            used_percent: percent,
            window_minutes: 300,
            resets_at: time(3600),
        }
    }
    fn event(seconds: i64, input: u64, output: u64) -> UsageEvent {
        UsageEvent {
            id: format!("response-{seconds}"),
            timestamp: time(seconds),
            session: "session".into(),
            model: "test-model".into(),
            provider: "openai".into(),
            tier: Some("standard".into()),
            project: None,
            usage: Usage {
                input,
                cached: Some(input),
                output,
                reasoning: Some(output),
                ..Usage::default()
            },
            source: "wsl".into(),
            origins: BTreeSet::new(),
            context_tokens: None,
            cache_retention: None,
        }
    }
    fn evidence() -> (Vec<QuotaSnapshot>, Vec<UsageEvent>) {
        (
            vec![sample(0, 80.0), sample(120, 82.0), sample(240, 84.0)],
            vec![event(60, 900, 100), event(180, 1800, 200)],
        )
    }

    #[test]
    fn parsing_validates_limits_and_does_not_retain_credit_balances() {
        let value = json!({"limit_id":"codex","primary":{"used_percent":42,"window_minutes":300,"resets_at":time(3600).timestamp()},"secondary":null,"credits":{"balance":"secret"}});
        let result = QuotaSnapshot::parse(time(0), &value, "windows");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, "windows");
        assert!(!serde_json::to_string(&result).unwrap().contains("secret"));
        for (key, invalid) in [
            ("used_percent", json!(-1)),
            ("used_percent", json!(101)),
            ("used_percent", Value::Null),
            ("window_minutes", json!(0)),
            ("window_minutes", json!(-1)),
            ("window_minutes", json!(10081)),
            ("resets_at", json!(time(-1).timestamp())),
            ("resets_at", json!(time(20000).timestamp())),
            ("resets_at", json!(i64::MAX)),
        ] {
            let mut value = value.clone();
            value["primary"][key] = invalid;
            assert!(QuotaSnapshot::parse(time(0), &value, "wsl").is_empty());
        }
    }

    #[test]
    fn estimate_counts_total_tokens_once_and_uses_snapshot_time_for_eta() {
        let (samples, events) = evidence();
        let summary = &summarize(&samples, &events, time(250), false)[0];
        assert_eq!(summary.forecast_status, "estimated");
        assert_eq!(summary.remaining_tokens, Some(12000));
        assert_eq!(summary.forecast_tokens, Some(3000));
        assert_eq!(summary.exhausts_at, Some(time(1200)));
        assert_eq!(summary.forecast_window_seconds, Some(240.0));
        assert_eq!(summary.forecast_used_percent, Some(4.0));
    }

    #[test]
    fn copied_snapshots_and_usage_do_not_inflate_estimates() {
        let (mut samples, mut events) = evidence();
        let expected = summarize(&samples, &events, time(240), false)[0].remaining_tokens;
        samples.extend(samples.clone());
        events.extend(events.clone());
        assert_eq!(
            summarize(&samples, &events, time(240), false)[0].remaining_tokens,
            expected
        );
    }

    #[test]
    fn sources_and_buckets_are_independent_and_reconciled_origins_are_usable() {
        let (mut samples, mut events) = evidence();
        let mut windows = samples.clone();
        for sample in &mut windows {
            sample.source = "windows".into();
            sample.bucket = "secondary".into();
        }
        samples.extend(windows);
        for event in &mut events {
            event.origins.insert("windows".into());
        }
        let result = summarize(&samples, &events, time(240), false);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|s| s.remaining_tokens == Some(12000)));
        events[0].origins.clear();
        assert!(summarize(&samples, &events, time(240), false)
            .iter()
            .find(|s| s.source == "windows")
            .unwrap()
            .remaining_tokens
            .is_none());
    }

    #[test]
    fn stale_expired_future_and_incomplete_data_do_not_generate_forecasts() {
        let (samples, events) = evidence();
        for (now, incomplete, expected) in [
            (time(541), false, "stale"),
            (time(3600), false, "expired"),
            (time(240), true, "incomplete_data"),
        ] {
            let summary = &summarize(&samples, &events, now, incomplete)[0];
            assert_eq!(summary.forecast_status, expected);
            assert!(summary.remaining_tokens.is_none());
            assert!(summary.exhausts_at.is_none());
        }
        assert!(summarize(&[sample(241, 99.0)], &events, time(240), false).is_empty());
    }

    #[test]
    fn reset_jitter_is_not_a_reset_and_previous_deadline_can_be_observed() {
        let mut first = sample(0, 80.0);
        first.resets_at += Duration::seconds(1);
        let latest = sample(240, 84.0);
        let result = summarize(&[first.clone(), latest.clone()], &[], time(240), false);
        assert_eq!(result[0].last_reset_basis, "estimated");
        assert_eq!(result[0].last_reset, Some(time(3600 - 18000)));
        first.timestamp = time(-18000);
        first.resets_at = time(3600 - 18000);
        let result = summarize(&[first, latest], &[], time(240), false);
        assert_eq!(result[0].last_reset_basis, "observed");
        assert_eq!(result[0].last_reset, Some(time(3600 - 18000)));
    }

    #[test]
    fn forecast_does_not_bridge_resets_percentage_drops_or_large_sample_gaps() {
        let (samples, events) = evidence();
        let mut changed = samples.clone();
        changed[1].used_percent = 79.0;
        assert_eq!(
            summarize(&changed, &events, time(240), false)[0].forecast_status,
            "quota_changed"
        );
        changed = samples.clone();
        changed[0].resets_at -= Duration::seconds(300);
        assert!(summarize(&changed, &events, time(240), false)[0]
            .remaining_tokens
            .is_none());
        changed = vec![sample(0, 80.0), sample(1800, 84.0)];
        assert_eq!(
            summarize(&changed, &events, time(1800), false)[0].forecast_status,
            "sparse_history"
        );
    }

    #[test]
    fn zero_progress_short_samples_missing_usage_and_changing_mix_are_unavailable() {
        let (samples, events) = evidence();
        for changed in [
            vec![sample(0, 80.0)],
            vec![sample(0, 80.0), sample(240, 80.0)],
            vec![sample(0, 80.0), sample(60, 84.0)],
        ] {
            assert!(summarize(&changed, &events, time(240), false)[0]
                .remaining_tokens
                .is_none());
        }
        assert_eq!(
            summarize(&samples, &[], time(240), false)[0].forecast_status,
            "missing_usage"
        );
        let mut events = events;
        events[1].model = "different-model".into();
        assert_eq!(
            summarize(&samples, &events, time(240), false)[0].forecast_status,
            "changing_workload"
        );
    }

    #[test]
    fn exhausted_resets_first_and_already_elapsed_predictions_are_distinguished() {
        let (mut samples, events) = evidence();
        samples[2].used_percent = 100.0;
        let summary = &summarize(&samples, &events, time(240), false)[0];
        assert_eq!(summary.forecast_status, "exhausted");
        assert_eq!(summary.remaining_tokens, Some(0));
        assert!(summary.exhausts_at.is_none());
        let (mut samples, events) = evidence();
        for sample in &mut samples {
            sample.resets_at = time(600);
        }
        let summary = &summarize(&samples, &events, time(240), false)[0];
        assert_eq!(summary.forecast_status, "resets_first");
        assert_eq!(summary.remaining_tokens, Some(12000));
        assert!(summary.exhausts_at.is_none());
        samples[0].used_percent = 80.0;
        samples[1].used_percent = 89.0;
        samples[2].used_percent = 99.0;
        let summary = &summarize(&samples, &events, time(260), false)[0];
        assert_eq!(summary.forecast_status, "awaiting_update");
        assert!(summary.remaining_tokens.is_none());
    }

    #[test]
    fn conflicting_duplicate_snapshots_or_usage_fail_closed() {
        let (mut samples, events) = evidence();
        samples.push(sample(120, 83.0));
        assert_eq!(
            summarize(&samples, &events, time(240), false)[0].forecast_status,
            "conflicting_snapshots"
        );
        let (mut samples, events) = evidence();
        samples.push(sample(240, 100.0));
        let summary = &summarize(&samples, &events, time(240), false)[0];
        assert_eq!(summary.forecast_status, "conflicting_snapshots");
        assert!(summary.remaining_tokens.is_none());
        let (samples, mut events) = evidence();
        let mut conflict = events[0].clone();
        conflict.usage.input += 1;
        events.push(conflict);
        assert_eq!(
            summarize(&samples, &events, time(240), false)[0].forecast_status,
            "inconsistent_usage"
        );
    }
}
