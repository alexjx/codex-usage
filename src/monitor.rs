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

#[derive(Default)]
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
        self.rate_over(now, Duration::from_secs(5))
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
        if seconds == 0.0 {
            return 0.0;
        }
        self.samples.iter().map(|(_, n)| *n as f64).sum::<f64>() / seconds
    }
}
pub struct Tracker {
    started: Instant,
    seen: BTreeSet<String>,
    readable: BTreeSet<String>,
    last_observed: chrono::DateTime<Utc>,
    previous_total: u64,
    previous_cost: Option<Decimal>,
    previous_period: String,
    delta: i128,
    cost_delta: Option<Decimal>,
    sequence: u64,
    speed: Speed,
    session_speeds: BTreeMap<String, Speed>,
}
impl Tracker {
    pub fn new(scan: &Scan, report: &Report, pricing_enabled: bool) -> Self {
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
            previous_period: report.period.label.clone(),
            delta: 0,
            cost_delta: pricing_enabled.then_some(Decimal::ZERO),
            sequence: 0,
            speed: Speed::default(),
            session_speeds: BTreeMap::new(),
        }
    }
    pub fn info(&mut self, scan_millis: u128) -> MonitorInfo {
        self.info_at(scan_millis, self.started.elapsed())
    }
    fn info_at(&mut self, scan_millis: u128, now: Duration) -> MonitorInfo {
        let mut session_output_tokens_per_second = BTreeMap::new();
        self.session_speeds.retain(|session, speed| {
            let rate = speed.rate_over(now, Duration::from_secs(30));
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
            output_tokens_per_second: self.speed.rate(now),
            average_session_output_tokens_per_second,
            session_output_tokens_per_second,
            session_speed_window_seconds: 30,
            scan_millis,
        }
    }
    pub fn observe(&mut self, cli: &Cli, scan: &Scan, report: &mut Report, scan_millis: u128) {
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
                    && e.timestamp >= self.last_observed - chrono::Duration::seconds(5)
                    && e.timestamp <= report.generated_at + chrono::Duration::seconds(5)
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
        if rollover {
            self.delta = report.totals.total as i128;
            self.cost_delta = self.cost_delta.map(|_| current);
            self.speed = Speed::default();
            self.session_speeds.clear();
        } else {
            self.delta += report.totals.total as i128 - self.previous_total as i128;
            self.cost_delta = self.cost_delta.and_then(|delta| {
                delta.checked_add(current.checked_sub(self.previous_cost.unwrap_or_default())?)
            });
        }
        self.speed.record(self.started.elapsed(), output);
        let now = self.started.elapsed();
        for (session, tokens) in session_output {
            self.session_speeds
                .entry(session)
                .or_default()
                .record(now, tokens);
        }
        self.previous_total = report.totals.total;
        self.previous_cost = self.cost_delta.map(|_| current);
        self.previous_period = report.period.label.clone();
        self.last_observed = report.generated_at;
        self.readable = current_readable;
        self.sequence += 1;
        report.monitor = Some(self.info(scan_millis));
        if late > 0 {
            report.coverage.messages.insert(format!(
                "{late} late/recovered records excluded from output speed"
            ));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_rate_expires() {
        let mut s = Speed::default();
        assert_eq!(s.rate(Duration::ZERO), 0.0);
        s.record(Duration::from_secs(1), 600);
        assert_eq!(s.rate(Duration::from_secs(1)), 600.);
        assert_eq!(s.rate(Duration::from_secs(2)), 300.);
        assert_eq!(s.rate(Duration::from_secs(6)), 0.);
    }

    #[test]
    fn session_average_smooths_parallel_output_and_expires_idle_sessions() {
        let mut tracker = Tracker::new(
            &Scan::default(),
            &super::regression_tests::report("today", 0, None),
            false,
        );
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
        let info = tracker.info_at(0, Duration::from_secs(20));
        assert_eq!(info.session_output_tokens_per_second["a"], 15.0);
        assert_eq!(info.session_output_tokens_per_second["b"], 30.0);
        assert_eq!(info.average_session_output_tokens_per_second, 22.5);
        // A quiet refresh preserves the rolling average; it does not average tick rates.
        let info = tracker.info_at(0, Duration::from_secs(30));
        assert_eq!(info.average_session_output_tokens_per_second, 15.0);
        let info = tracker.info_at(0, Duration::from_secs(31));
        assert_eq!(info.session_output_tokens_per_second.len(), 1);
        assert_eq!(info.average_session_output_tokens_per_second, 20.0);
        let info = tracker.info_at(0, Duration::from_secs(40));
        assert_eq!(info.average_session_output_tokens_per_second, 0.0);
        assert!(info.session_output_tokens_per_second.is_empty());
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
