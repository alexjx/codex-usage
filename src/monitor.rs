use crate::{
    cli::Cli,
    model::Scan,
    report::{self, MonitorInfo, Report},
};
use chrono::Utc;
use rust_decimal::Decimal;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

const TIMESTAMP_JITTER_SECONDS: i64 = 5;
const SESSION_SPEED_WINDOW_SECONDS: u64 = 60;

#[derive(Default)]
/// Rolling output samples used only for the independent per-session average.
pub struct Speed {
    samples: VecDeque<(Duration, u64)>,
}
impl Speed {
    pub fn record(&mut self, time: Duration, output: u64) {
        if output > 0 {
            self.samples.push_back((time, output));
        }
    }
    pub fn rate(&mut self, now: Duration) -> f64 {
        self.rate_over(now, Duration::from_secs(SESSION_SPEED_WINDOW_SECONDS))
    }
    fn rate_over(&mut self, now: Duration, window: Duration) -> f64 {
        if now >= window {
            while self
                .samples
                .front()
                .is_some_and(|(t, _)| *t <= now - window)
            {
                self.samples.pop_front();
            }
        }
        let seconds = now.min(window).as_secs_f64();
        if seconds == 0.0 || self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().map(|(_, n)| *n as f64).sum::<f64>() / seconds
    }
}

#[derive(Default)]
struct Activity {
    tokens: i128,
    cost: Option<Decimal>,
    cost_partial: bool,
    output_per_second: f64,
    sample_seconds: f64,
}
pub struct Tracker {
    started: Instant,
    seen: BTreeSet<String>,
    readable: BTreeSet<String>,
    last_observed: chrono::DateTime<Utc>,
    previous_total: u64,
    previous_cost: Option<Decimal>,
    previous_priced_tokens: u64,
    previous_unpriced_tokens: u64,
    has_sources: bool,
    previous_period: String,
    delta: i128,
    cost_delta: Option<Decimal>,
    sequence: u64,
    interval: Duration,
    last_sample: Duration,
    activity: Activity,
    session_speeds: BTreeMap<String, Speed>,
}
impl Tracker {
    pub fn new(scan: &Scan, report: &Report, pricing_enabled: bool) -> Self {
        Self::with_interval(scan, report, pricing_enabled, Duration::from_secs(2))
    }
    pub fn with_interval(
        scan: &Scan,
        report: &Report,
        pricing_enabled: bool,
        interval: Duration,
    ) -> Self {
        Self {
            started: Instant::now(),
            seen: scan.events.iter().map(|e| e.id.clone()).collect(),
            readable: scan
                .sources
                .iter()
                .filter(|s| s.readable)
                .map(|s| s.name.clone())
                .collect(),
            last_observed: report.generated_at,
            previous_total: report.totals.total,
            previous_cost: pricing_enabled.then(|| report.totals.cost_usd.unwrap_or_default()),
            previous_priced_tokens: report.totals.priced_tokens,
            previous_unpriced_tokens: report.totals.unpriced_tokens,
            has_sources: !scan.sources.is_empty(),
            previous_period: report.period.label.clone(),
            delta: 0,
            cost_delta: pricing_enabled.then_some(Decimal::ZERO),
            sequence: 0,
            interval,
            last_sample: Duration::ZERO,
            activity: Activity {
                cost: pricing_enabled.then_some(Decimal::ZERO),
                ..Default::default()
            },
            session_speeds: BTreeMap::new(),
        }
    }
    pub fn info(&mut self, scan_millis: u128) -> MonitorInfo {
        self.info_at(scan_millis, self.started.elapsed())
    }
    fn info_at(&mut self, scan_millis: u128, now: Duration) -> MonitorInfo {
        let pricing_enabled = self.cost_delta.is_some();
        let mut window_cost_usd = self.activity.cost;
        let mut window_cost_partial = self.activity.cost_partial;
        if pricing_enabled && self.has_sources && self.readable.is_empty() {
            window_cost_partial = true;
            if window_cost_usd.is_none_or(|cost| cost.is_zero()) {
                window_cost_usd = None;
            }
        }
        let mut session_output_tokens_per_second = BTreeMap::new();
        self.session_speeds.retain(|session, speed| {
            let rate = speed.rate(now);
            if rate > 0.0 {
                session_output_tokens_per_second.insert(session.clone(), rate);
                true
            } else {
                false
            }
        });
        let average_session_output_tokens_per_second =
            if session_output_tokens_per_second.is_empty() {
                0.0
            } else {
                session_output_tokens_per_second.values().sum::<f64>()
                    / session_output_tokens_per_second.len() as f64
            };
        MonitorInfo {
            sequence: self.sequence,
            final_snapshot: false,
            since_ready_tokens: self.delta,
            since_ready_cost_usd: self.cost_delta,
            window_tokens: self.activity.tokens,
            window_cost_usd,
            window_cost_partial,
            window_seconds: self.interval.as_secs_f64(),
            sample_seconds: self.activity.sample_seconds,
            output_tokens_per_second: self.activity.output_per_second,
            average_session_output_tokens_per_second,
            session_output_tokens_per_second,
            session_speed_window_seconds: SESSION_SPEED_WINDOW_SECONDS,
            scan_millis,
        }
    }
    pub fn observe(&mut self, cli: &Cli, scan: &Scan, report: &mut Report, scan_millis: u128) {
        self.observe_at(cli, scan, report, scan_millis, self.started.elapsed());
    }
    fn observe_at(
        &mut self,
        cli: &Cli,
        scan: &Scan,
        report: &mut Report,
        scan_millis: u128,
        now: Duration,
    ) {
        self.interval = cli.interval();
        let current_readable: BTreeSet<_> = scan
            .sources
            .iter()
            .filter(|s| s.readable)
            .map(|s| s.name.clone())
            .collect();
        let mut output = 0u64;
        let mut session_output = BTreeMap::<String, u64>::new();
        let mut late = 0;
        for e in &scan.events {
            if self.seen.insert(e.id.clone()) && report::matches(cli, &report.period, e) {
                if self.readable.contains(&e.source)
                    && current_readable.contains(&e.source)
                    && e.timestamp
                        >= self.last_observed - chrono::Duration::seconds(TIMESTAMP_JITTER_SECONDS)
                    && e.timestamp
                        <= report.generated_at + chrono::Duration::seconds(TIMESTAMP_JITTER_SECONDS)
                {
                    output = output.saturating_add(e.usage.output);
                    let tokens = session_output.entry(e.session.clone()).or_default();
                    *tokens = tokens.saturating_add(e.usage.output);
                } else {
                    late += 1;
                }
            }
        }
        let rollover = report.period.label != self.previous_period;
        let current = report.totals.cost_usd.unwrap_or_default();
        let tokens = if rollover {
            report.totals.total as i128
        } else {
            report.totals.total as i128 - self.previous_total as i128
        };
        let cost = self.cost_delta.and_then(|_| {
            if rollover {
                Some(current)
            } else {
                current.checked_sub(self.previous_cost.unwrap_or_default())
            }
        });
        let priced_changed = if rollover {
            report.totals.priced_tokens != 0
        } else {
            report.totals.priced_tokens != self.previous_priced_tokens
        };
        let unpriced_changed = if rollover {
            report.totals.unpriced_tokens != 0
        } else {
            report.totals.unpriced_tokens != self.previous_unpriced_tokens
        };
        if rollover {
            self.delta = report.totals.total as i128;
            self.cost_delta = self.cost_delta.map(|_| current);
            self.session_speeds.clear();
        } else {
            self.delta += report.totals.total as i128 - self.previous_total as i128;
            self.cost_delta = self.cost_delta.and_then(|delta| {
                delta.checked_add(current.checked_sub(self.previous_cost.unwrap_or_default())?)
            });
        }
        let sample_seconds = now.saturating_sub(self.last_sample).as_secs_f64();
        let cost_partial = self.cost_delta.is_some() && unpriced_changed;
        let known_cost = priced_changed || cost.is_some_and(|cost| !cost.is_zero());
        self.activity = Activity {
            tokens,
            cost: if cost_partial && !known_cost {
                None
            } else {
                cost
            },
            cost_partial,
            output_per_second: if sample_seconds > 0.0 {
                output as f64 / sample_seconds
            } else {
                0.0
            },
            sample_seconds,
        };
        self.last_sample = now;
        for (session, tokens) in session_output {
            self.session_speeds
                .entry(session)
                .or_default()
                .record(now, tokens);
        }
        self.previous_total = report.totals.total;
        self.previous_cost = self.cost_delta.map(|_| current);
        self.previous_priced_tokens = report.totals.priced_tokens;
        self.previous_unpriced_tokens = report.totals.unpriced_tokens;
        self.has_sources |= !scan.sources.is_empty();
        self.previous_period = report.period.label.clone();
        self.last_observed = report.generated_at;
        self.readable = current_readable;
        self.sequence += 1;
        report.monitor = Some(self.info_at(scan_millis, now));
        if late > 0 {
            report.coverage.messages.insert(format!(
                "{late} late/recovered records excluded from output speed"
            ));
        }
    }
}
#[cfg(test)]
mod interval_tests {
    use super::regression_tests::report;
    use super::*;
    use crate::model::{SourceStatus, Usage, UsageEvent};
    use clap::Parser;

    fn cli(interval: &str) -> Cli {
        Cli::parse_from(["codex-usage", "monitor", "--interval", interval])
    }
    fn scan(readable: bool) -> Scan {
        Scan {
            sources: vec![SourceStatus {
                name: "local".into(),
                root: "/not-read".into(),
                readable,
                last_success: None,
                error: None,
            }],
            ..Default::default()
        }
    }
    fn counts(day: &str, total: u64, priced: u64, unpriced: u64, cost: Option<Decimal>) -> Report {
        let mut r = report(day, total, cost);
        r.totals.priced_tokens = priced;
        r.totals.unpriced_tokens = unpriced;
        r
    }
    fn event(id: &str, output: u64, time: chrono::DateTime<Utc>) -> UsageEvent {
        UsageEvent {
            id: id.into(),
            timestamp: time,
            session: "s".into(),
            model: "test".into(),
            provider: "openai".into(),
            tier: Some("standard".into()),
            project: None,
            usage: Usage {
                input: 10,
                output,
                ..Default::default()
            },
            source: "local".into(),
            origins: BTreeSet::new(),
            context_tokens: None,
            cache_retention: None,
        }
    }

    #[test]
    fn configured_intervals_label_samples_and_rates_use_actual_elapsed_time() {
        for (value, configured, actual) in [
            ("1s", Duration::from_secs(1), Duration::from_secs(1)),
            ("2s", Duration::from_secs(2), Duration::from_secs(2)),
            ("5s", Duration::from_secs(5), Duration::from_secs(7)),
            (
                "1500ms",
                Duration::from_millis(1500),
                Duration::from_millis(2500),
            ),
        ] {
            let cli = cli(value);
            let mut scan = scan(true);
            let baseline = report("day1", 0, None);
            let mut tracker = Tracker::with_interval(&scan, &baseline, true, configured);
            let startup = tracker.info_at(0, Duration::ZERO);
            assert_eq!(startup.window_seconds, configured.as_secs_f64(), "{value}");
            assert_eq!(startup.sample_seconds, 0.0);
            assert_eq!(startup.window_tokens, 0);
            assert_eq!(startup.output_tokens_per_second, 0.0);
            let mut first = counts("day1", 100, 100, 0, Some(Decimal::new(1, 2)));
            first.generated_at =
                baseline.generated_at + chrono::Duration::from_std(actual).unwrap();
            scan.events.push(event("first", 90, first.generated_at));
            tracker.observe_at(&cli, &scan, &mut first, 0, actual);
            let info = first.monitor.as_ref().unwrap();
            assert_eq!(info.window_seconds, configured.as_secs_f64());
            assert_eq!(info.sample_seconds, actual.as_secs_f64());
            assert_eq!(info.window_tokens, 100);
            assert_eq!(info.window_cost_usd, Some(Decimal::new(1, 2)));
            assert_eq!(info.output_tokens_per_second, 90.0 / actual.as_secs_f64());
            let mut second = counts("day1", 140, 140, 0, Some(Decimal::new(14, 3)));
            second.generated_at = first.generated_at + chrono::Duration::from_std(actual).unwrap();
            scan.events.push(event("second", 30, second.generated_at));
            tracker.observe_at(&cli, &scan, &mut second, 0, actual * 2);
            let info = second.monitor.as_ref().unwrap();
            assert_eq!(
                info.window_tokens, 40,
                "latest interval must not contain prior intervals"
            );
            assert_eq!(info.window_cost_usd, Some(Decimal::new(4, 3)));
            assert_eq!(info.since_ready_tokens, 140);
            assert_eq!(info.output_tokens_per_second, 30.0 / actual.as_secs_f64());
            let mut idle = second.clone();
            idle.generated_at += chrono::Duration::from_std(actual).unwrap();
            tracker.observe_at(&cli, &scan, &mut idle, 0, actual * 3);
            let info = idle.monitor.as_ref().unwrap();
            assert_eq!(
                info.window_tokens, 0,
                "idle next observation must clear deltas: {value}"
            );
            assert_eq!(info.window_cost_usd, Some(Decimal::ZERO));
            assert_eq!(info.output_tokens_per_second, 0.0);
            assert!(!info.output_tokens_per_second.is_sign_negative());
            assert!(info.average_session_output_tokens_per_second > 0.0);
            assert_eq!(info.since_ready_tokens, 140);
            assert_eq!(info.since_ready_cost_usd, Some(Decimal::new(14, 3)));
        }
    }

    #[test]
    fn observe_updates_configured_interval_and_zero_elapsed_does_not_divide_by_zero() {
        let mut scan = scan(true);
        let baseline = report("day1", 0, None);
        let mut tracker = Tracker::new(&scan, &baseline, true);
        assert_eq!(tracker.info(0).window_seconds, 2.0);
        let mut first = counts("day1", 100, 100, 0, Some(Decimal::new(1, 2)));
        scan.events.push(event("r", 90, first.generated_at));
        tracker.observe_at(&cli("1500ms"), &scan, &mut first, 0, Duration::ZERO);
        let info = first.monitor.as_ref().unwrap();
        assert_eq!(info.window_seconds, 1.5);
        assert_eq!(info.sample_seconds, 0.0);
        assert_eq!(info.output_tokens_per_second, 0.0);
        assert_eq!(info.window_tokens, 100);
    }

    #[test]
    fn each_observation_handles_signed_and_cost_only_corrections() {
        let scan = scan(true);
        let baseline = counts("day1", 100, 100, 0, Some(Decimal::new(1, 2)));
        let mut tracker = Tracker::new(&scan, &baseline, true);
        let mut correction = counts("day1", 60, 60, 0, Some(Decimal::new(6, 3)));
        tracker.observe_at(
            &cli("2s"),
            &scan,
            &mut correction,
            0,
            Duration::from_secs(2),
        );
        let info = correction.monitor.as_ref().unwrap();
        assert_eq!(info.window_tokens, -40);
        assert_eq!(info.window_cost_usd, Some(Decimal::new(-4, 3)));
        assert!(!info.window_cost_partial);
        let mut repriced = counts("day1", 60, 60, 0, Some(Decimal::new(8, 3)));
        tracker.observe_at(&cli("2s"), &scan, &mut repriced, 0, Duration::from_secs(4));
        let info = repriced.monitor.as_ref().unwrap();
        assert_eq!(info.window_tokens, 0);
        assert_eq!(info.window_cost_usd, Some(Decimal::new(2, 3)));
        assert_eq!(info.since_ready_tokens, -40);
        assert_eq!(info.since_ready_cost_usd, Some(Decimal::new(-2, 3)));
    }

    #[test]
    fn price_coverage_belongs_only_to_the_latest_observation() {
        let scan = scan(true);
        let baseline = counts("day1", 100, 0, 100, None);
        let mut tracker = Tracker::new(&scan, &baseline, true);
        assert!(!tracker.info(0).window_cost_partial);
        let mut unknown = counts("day1", 120, 0, 120, None);
        tracker.observe_at(&cli("1s"), &scan, &mut unknown, 0, Duration::from_secs(1));
        assert_eq!(unknown.monitor.as_ref().unwrap().window_tokens, 20);
        assert!(unknown.monitor.as_ref().unwrap().window_cost_usd.is_none());
        assert!(unknown.monitor.as_ref().unwrap().window_cost_partial);
        let mut priced = counts("day1", 130, 10, 120, Some(Decimal::new(1, 2)));
        tracker.observe_at(&cli("1s"), &scan, &mut priced, 0, Duration::from_secs(2));
        assert_eq!(priced.monitor.as_ref().unwrap().window_tokens, 10);
        assert_eq!(
            priced.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::new(1, 2))
        );
        assert!(
            !priced.monitor.as_ref().unwrap().window_cost_partial,
            "prior unknown activity must not contaminate a new priced sample"
        );
        let mut mixed = counts("day1", 160, 30, 130, Some(Decimal::new(3, 2)));
        tracker.observe_at(&cli("1s"), &scan, &mut mixed, 0, Duration::from_secs(3));
        assert_eq!(mixed.monitor.as_ref().unwrap().window_tokens, 30);
        assert_eq!(
            mixed.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::new(2, 2))
        );
        assert!(mixed.monitor.as_ref().unwrap().window_cost_partial);
        let mut resolved = counts("day1", 160, 160, 0, Some(Decimal::new(4, 2)));
        tracker.observe_at(&cli("1s"), &scan, &mut resolved, 0, Duration::from_secs(4));
        assert_eq!(resolved.monitor.as_ref().unwrap().window_tokens, 0);
        assert_eq!(
            resolved.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::new(1, 2))
        );
        assert!(resolved.monitor.as_ref().unwrap().window_cost_partial);
        tracker.observe_at(&cli("1s"), &scan, &mut resolved, 0, Duration::from_secs(5));
        assert_eq!(
            resolved.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::ZERO)
        );
        assert!(!resolved.monitor.as_ref().unwrap().window_cost_partial);
        let mut unknown = counts("day1", 161, 160, 1, Some(Decimal::new(4, 2)));
        tracker.observe_at(&cli("1s"), &scan, &mut unknown, 0, Duration::from_secs(6));
        let mut zero_rate = counts("day1", 161, 161, 0, Some(Decimal::new(4, 2)));
        tracker.observe_at(&cli("1s"), &scan, &mut zero_rate, 0, Duration::from_secs(7));
        assert_eq!(zero_rate.monitor.as_ref().unwrap().window_tokens, 0);
        assert_eq!(
            zero_rate.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::ZERO)
        );
        assert!(zero_rate.monitor.as_ref().unwrap().window_cost_partial);
    }

    #[test]
    fn midnight_restarts_cumulative_changes_without_carrying_previous_activity() {
        let scan = scan(true);
        let baseline = counts("day1", 100, 100, 0, Some(Decimal::new(1, 2)));
        let mut tracker = Tracker::new(&scan, &baseline, true);
        let mut unknown = counts("day1", 120, 100, 20, Some(Decimal::new(1, 2)));
        tracker.observe_at(&cli("2s"), &scan, &mut unknown, 0, Duration::from_secs(2));
        assert!(unknown.monitor.as_ref().unwrap().window_cost_partial);
        tracker
            .session_speeds
            .entry("yesterday".into())
            .or_default()
            .record(Duration::from_secs(2), 300);
        let mut next_day = counts("day2", 15, 15, 0, Some(Decimal::new(1, 3)));
        tracker.observe_at(&cli("2s"), &scan, &mut next_day, 0, Duration::from_secs(4));
        let info = next_day.monitor.as_ref().unwrap();
        assert_eq!(info.window_tokens, 15);
        assert_eq!(info.window_cost_usd, Some(Decimal::new(1, 3)));
        assert_eq!(info.since_ready_tokens, 15);
        assert_eq!(info.since_ready_cost_usd, Some(Decimal::new(1, 3)));
        assert!(!info.window_cost_partial);
        assert!(info.session_output_tokens_per_second.is_empty());
        let mut empty_day = counts("day3", 0, 0, 0, None);
        tracker.observe_at(&cli("2s"), &scan, &mut empty_day, 0, Duration::from_secs(6));
        assert_eq!(empty_day.monitor.as_ref().unwrap().window_tokens, 0);
        assert_eq!(
            empty_day.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::ZERO)
        );
    }

    #[test]
    fn unreadable_sources_and_disabled_pricing_keep_explicit_cost_states() {
        let mut scan = scan(false);
        let mut r = report("day1", 0, None);
        let mut tracker = Tracker::new(&scan, &r, true);
        assert!(tracker.info(0).window_cost_usd.is_none());
        assert!(tracker.info(0).window_cost_partial);
        scan.sources[0].readable = true;
        tracker.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(2));
        assert_eq!(
            r.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::ZERO)
        );
        r.totals.total = 20;
        r.totals.priced_tokens = 20;
        r.totals.cost_usd = Some(Decimal::new(1, 2));
        tracker.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(4));
        assert_eq!(
            r.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::new(1, 2))
        );
        scan.sources[0].readable = false;
        tracker.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(6));
        assert_eq!(r.monitor.as_ref().unwrap().window_tokens, 0);
        assert!(r.monitor.as_ref().unwrap().window_cost_usd.is_none());
        assert!(r.monitor.as_ref().unwrap().window_cost_partial);
        let mut disabled = Tracker::new(&scan, &r, false);
        r.totals.total = 23;
        r.totals.unpriced_tokens = 3;
        disabled.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(2));
        assert!(r.monitor.as_ref().unwrap().window_cost_usd.is_none());
        assert!(!r.monitor.as_ref().unwrap().window_cost_partial);
    }

    #[test]
    fn session_average_keeps_its_independent_sixty_second_window() {
        let mut s = Speed::default();
        assert_eq!(s.rate(Duration::ZERO), 0.0);
        s.record(Duration::from_secs(1), 600);
        assert_eq!(s.rate(Duration::from_secs(1)), 600.0);
        assert_eq!(s.rate(Duration::from_secs(2)), 300.0);
        assert!(s.rate(Duration::from_secs(31)) > 0.0);
        assert_eq!(s.rate(Duration::from_secs(60)), 10.0);
        assert_eq!(s.rate(Duration::from_secs(61)), 0.0);
        assert!(!s.rate(Duration::from_secs(61)).is_sign_negative());
        let mut tracker = Tracker::new(&Scan::default(), &report("day1", 0, None), false);
        tracker
            .session_speeds
            .entry("a".into())
            .or_default()
            .record(Duration::from_secs(1), 300);
        tracker
            .session_speeds
            .entry("b".into())
            .or_default()
            .record(Duration::from_secs(10), 600);
        assert_eq!(
            tracker
                .info_at(0, Duration::from_secs(20))
                .average_session_output_tokens_per_second,
            22.5
        );
        assert_eq!(
            tracker
                .info_at(0, Duration::from_secs(30))
                .average_session_output_tokens_per_second,
            15.0
        );
        assert_eq!(
            tracker
                .info_at(0, Duration::from_secs(60))
                .average_session_output_tokens_per_second,
            7.5
        );
        assert_eq!(
            tracker
                .info_at(0, Duration::from_secs(61))
                .average_session_output_tokens_per_second,
            10.0
        );
        assert_eq!(
            tracker
                .info_at(0, Duration::from_secs(70))
                .average_session_output_tokens_per_second,
            0.0
        );
        assert!(tracker.session_speeds.is_empty());
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use crate::{
        model::{Coverage, SourceStatus, Usage, UsageEvent},
        report::{Counts, Period},
    };
    use clap::Parser;
    use std::collections::BTreeSet;
    pub(super) fn report(period: &str, total: u64, cost: Option<Decimal>) -> Report {
        Report {
            schema_version: 1,
            generated_at: Utc::now(),
            kind: "Monitor".into(),
            period: Period {
                label: period.into(),
                timezone: "UTC".into(),
                start: None,
                end: None,
            },
            rows: Vec::new(),
            totals: Counts {
                total,
                cost_usd: cost,
                ..Default::default()
            },
            sources: Vec::new(),
            coverage: Coverage::default(),
            pricing: serde_json::json!({}),
            monitor: None,
        }
    }
    #[test]
    fn empty_start_and_empty_midnight_keep_cost_delta_available() {
        let cli = Cli::parse_from(["codex-usage", "monitor"]);
        let scan = Scan::default();
        let mut tracker = Tracker::new(&scan, &report("day1", 0, None), true);
        let mut first = report("day1", 100, Some(Decimal::new(38, 2)));
        tracker.observe(&cli, &scan, &mut first, 0);
        assert_eq!(
            first.monitor.unwrap().since_ready_cost_usd,
            Some(Decimal::new(38, 2))
        );
        let mut midnight = report("day2", 0, None);
        tracker
            .session_speeds
            .entry("yesterday".into())
            .or_default()
            .record(Duration::ZERO, 999);
        tracker.observe(&cli, &scan, &mut midnight, 0);
        assert!(midnight
            .monitor
            .as_ref()
            .unwrap()
            .session_output_tokens_per_second
            .is_empty());
        assert_eq!(
            midnight.monitor.as_ref().unwrap().since_ready_cost_usd,
            Some(Decimal::ZERO)
        );
        assert_eq!(midnight.monitor.as_ref().unwrap().window_tokens, 0);
        assert_eq!(
            midnight.monitor.as_ref().unwrap().window_cost_usd,
            Some(Decimal::ZERO)
        );
        assert_eq!(midnight.monitor.unwrap().since_ready_tokens, 0);
        let mut next = report("day2", 50, Some(Decimal::new(9, 2)));
        tracker.observe(&cli, &scan, &mut next, 0);
        assert_eq!(
            next.monitor.unwrap().since_ready_cost_usd,
            Some(Decimal::new(9, 2))
        );
    }
    #[test]
    fn startup_history_same_id_corrections_and_recovery_do_not_inflate_speed() {
        let cli = Cli::parse_from(["codex-usage", "monitor", "--no-cost"]);
        let event = UsageEvent {
            id: "request".into(),
            timestamp: Utc::now(),
            session: "s".into(),
            model: "test".into(),
            provider: "openai".into(),
            tier: None,
            project: None,
            usage: Usage {
                input: 10,
                output: 5,
                ..Default::default()
            },
            source: "local".into(),
            origins: BTreeSet::new(),
            context_tokens: None,
            cache_retention: None,
        };
        let status = SourceStatus {
            name: "local".into(),
            root: "/not-read".into(),
            readable: false,
            last_success: None,
            error: None,
        };
        let mut scan = Scan {
            sources: vec![status],
            ..Default::default()
        };
        let mut r = report("all", 0, None);
        let mut tracker = Tracker::new(&scan, &r, false);
        scan.sources[0].readable = true;
        scan.events.push(event.clone());
        tracker.observe(&cli, &scan, &mut r, 0);
        assert_eq!(r.monitor.as_ref().unwrap().output_tokens_per_second, 0.0);
        assert!(r
            .monitor
            .as_ref()
            .unwrap()
            .session_output_tokens_per_second
            .is_empty());
        scan.events[0].usage.output = 10;
        tracker.observe(&cli, &scan, &mut r, 0);
        assert_eq!(r.monitor.as_ref().unwrap().output_tokens_per_second, 0.0);
        assert!(r
            .monitor
            .as_ref()
            .unwrap()
            .session_output_tokens_per_second
            .is_empty());
        let mut fresh = event;
        fresh.id = "fresh".into();
        scan.events.push(fresh);
        tracker.observe(&cli, &scan, &mut r, 0);
        assert!(r.monitor.as_ref().unwrap().output_tokens_per_second > 0.0);
        assert!(
            r.monitor
                .as_ref()
                .unwrap()
                .average_session_output_tokens_per_second
                > 0.0
        );
        assert_eq!(
            r.monitor
                .as_ref()
                .unwrap()
                .session_output_tokens_per_second
                .len(),
            1
        );
        assert!(r.monitor.unwrap().since_ready_cost_usd.is_none());
    }
}
