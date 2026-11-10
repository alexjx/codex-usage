use crate::{
    cli::Cli,
    model::{AgentKind, EventKey, Scan, SessionKey},
    report::{self, AgentActivity, MonitorInfo, Report, Selection, SessionSpeed},
};
use chrono::Utc;
use rust_decimal::Decimal;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

const TIMESTAMP_JITTER_SECONDS: i64 = 5;
const SESSION_SPEED_WINDOW_SECONDS: u64 = 60;
const CHART_SPEED_WINDOW_SECONDS: u64 = 30;
const SPEED_HISTORY_CAPACITY: usize = 60;

#[derive(Default)]
/// Time-weighted output rate over a rolling window, including idle time.
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
#[derive(Default)]
struct AgentChart {
    speed: Speed,
    started: Duration,
    offset: Duration,
    history: VecDeque<Option<f64>>,
}
pub struct Tracker {
    started: Instant,
    seen: BTreeSet<EventKey>,
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
    session_speeds: BTreeMap<SessionKey, Speed>,
    agent_totals: BTreeMap<AgentKind, u64>,
    agent_since_ready: BTreeMap<AgentKind, i128>,
    agent_window: BTreeMap<AgentKind, i128>,
    agent_rate: BTreeMap<AgentKind, f64>,
    agent_charts: BTreeMap<AgentKind, AgentChart>,
    chart_speed: Speed,
    chart_started: Duration,
    chart_offset: Duration,
    output_speed_history: VecDeque<Option<f64>>,
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
            // Excluded but identified records are already observed; a later repair
            // must change totals without masquerading as fresh activity.
            seen: scan
                .observed_event_ids
                .iter()
                .cloned()
                .chain(scan.events.iter().map(|event| event.id.clone()))
                .collect(),
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
            agent_totals: BTreeMap::new(),
            agent_since_ready: scan
                .sources
                .iter()
                .map(|source| (source.agent, 0))
                .collect(),
            agent_window: BTreeMap::new(),
            agent_rate: BTreeMap::new(),
            agent_charts: BTreeMap::new(),
            chart_speed: Speed::default(),
            chart_started: Duration::ZERO,
            chart_offset: Duration::ZERO,
            output_speed_history: VecDeque::new(),
        }
    }
    /// Reconstruct the chart from reconciled records already loaded at startup.
    /// Historical tokens remain in `seen`, so they cannot become fresh activity.
    pub fn seed_chart(&mut self, cli: &Cli, scan: &Scan, report: &Report) {
        let end = report.generated_at;
        let window = Duration::from_secs(CHART_SPEED_WINDOW_SECONDS);
        let span = self.interval * (SPEED_HISTORY_CAPACITY as u32 - 1);
        let earliest = end - chrono::Duration::from_std(span + window).unwrap();
        let origin = earliest.max(report.period.start.unwrap_or(earliest));
        let elapsed = (end - origin).to_std().unwrap_or_default();
        self.chart_offset = elapsed;
        self.chart_started = Duration::ZERO;
        self.chart_speed = Speed::default();
        self.output_speed_history.clear();
        let selection = Selection::resolve(cli, scan).unwrap_or_default();
        let mut events: Vec<_> = scan
            .events
            .iter()
            .filter(|e| {
                e.timestamp >= origin
                    && e.timestamp <= end
                    && (e.origins.is_empty() && self.readable.contains(&e.source)
                        || e.origins
                            .iter()
                            .any(|origin| self.readable.contains(origin)))
                    && report::matches(cli, &report.period, e, &selection)
            })
            .collect();
        events.sort_unstable_by_key(|e| e.timestamp);
        let mut stream = events.iter().copied().peekable();
        for index in (0..SPEED_HISTORY_CAPACITY as u32).rev() {
            let Some(point) = elapsed.checked_sub(self.interval * index) else {
                continue;
            };
            let timestamp = origin + chrono::Duration::from_std(point).unwrap();
            while stream.peek().is_some_and(|e| e.timestamp <= timestamp) {
                let e = stream.next().unwrap();
                self.chart_speed
                    .record((e.timestamp - origin).to_std().unwrap(), e.usage.output);
            }
            self.output_speed_history
                .push_back(if self.has_sources && self.readable.is_empty() {
                    None
                } else {
                    Some(self.chart_speed.rate_over(point, window))
                });
        }
        // Reconstruct an independent chart for each selected agent.
        self.agent_charts.clear();
        let agents: BTreeSet<AgentKind> = scan.sources.iter().map(|source| source.agent).collect();
        for agent in agents {
            let has_sources = scan.sources.iter().any(|source| source.agent == agent);
            let agent_readable = scan
                .sources
                .iter()
                .any(|source| source.agent == agent && source.readable);
            let mut chart = AgentChart {
                offset: elapsed,
                ..Default::default()
            };
            let mut stream = events
                .iter()
                .copied()
                .filter(|e| e.id.agent == agent)
                .peekable();
            for index in (0..SPEED_HISTORY_CAPACITY as u32).rev() {
                let Some(point) = elapsed.checked_sub(self.interval * index) else {
                    continue;
                };
                let timestamp = origin + chrono::Duration::from_std(point).unwrap();
                while stream.peek().is_some_and(|e| e.timestamp <= timestamp) {
                    let e = stream.next().unwrap();
                    chart
                        .speed
                        .record((e.timestamp - origin).to_std().unwrap(), e.usage.output);
                }
                chart.history.push_back(if has_sources && !agent_readable {
                    None
                } else {
                    Some(chart.speed.rate_over(point, window))
                });
            }
            self.agent_charts.insert(agent, chart);
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
        let mut active_sessions = BTreeMap::new();
        self.session_speeds.retain(|session, speed| {
            let rate = speed.rate(now);
            if rate > 0.0 {
                active_sessions.insert(session.clone(), rate);
                true
            } else {
                false
            }
        });
        let average_session_output_tokens_per_second = if active_sessions.is_empty() {
            0.0
        } else {
            active_sessions.values().sum::<f64>() / active_sessions.len() as f64
        };
        let session_output_tokens_per_second: Vec<SessionSpeed> = active_sessions
            .into_iter()
            .map(|(session, output_tokens_per_second)| SessionSpeed {
                session,
                output_tokens_per_second,
            })
            .collect();
        let agent_activity: Vec<AgentActivity> = self
            .agent_since_ready
            .iter()
            .map(|(agent, since_ready)| AgentActivity {
                agent: *agent,
                since_ready_tokens: *since_ready,
                window_tokens: self.agent_window.get(agent).copied().unwrap_or_default(),
                output_tokens_per_second: self.agent_rate.get(agent).copied().unwrap_or_default(),
                output_speed_history: self
                    .agent_charts
                    .get(agent)
                    .map(|chart| chart.history.iter().copied().collect())
                    .unwrap_or_default(),
                output_speed_window_seconds: CHART_SPEED_WINDOW_SECONDS,
            })
            .collect();
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
            output_speed_history: self.output_speed_history.iter().copied().collect(),
            output_speed_window_seconds: CHART_SPEED_WINDOW_SECONDS,
            average_session_output_tokens_per_second,
            session_output_tokens_per_second,
            session_speed_window_seconds: SESSION_SPEED_WINDOW_SECONDS,
            agent_activity,
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
        let selection = Selection::resolve(cli, scan).unwrap_or_default();
        let current_readable: BTreeSet<_> = scan
            .sources
            .iter()
            .filter(|s| s.readable)
            .map(|s| s.name.clone())
            .collect();
        let readable_agent: BTreeSet<AgentKind> = scan
            .sources
            .iter()
            .filter(|s| s.readable)
            .map(|s| s.agent)
            .collect();
        let mut output = 0u64;
        let mut session_output = BTreeMap::<SessionKey, u64>::new();
        let mut agent_output = BTreeMap::<AgentKind, u64>::new();
        let mut agent_totals = BTreeMap::<AgentKind, u64>::new();
        let mut late = 0;
        for e in &scan.events {
            let new = self.seen.insert(e.id.clone());
            if !report::matches(cli, &report.period, e, &selection) {
                continue;
            }
            let total = agent_totals.entry(e.id.agent).or_default();
            *total = total.saturating_add(e.usage.total());
            if new {
                // Fresh activity requires an origin readable both before and now.
                let available = if e.origins.is_empty() {
                    self.readable.contains(&e.source) && current_readable.contains(&e.source)
                } else {
                    e.origins.iter().any(|origin| {
                        self.readable.contains(origin) && current_readable.contains(origin)
                    })
                };
                if available
                    && e.timestamp
                        >= self.last_observed - chrono::Duration::seconds(TIMESTAMP_JITTER_SECONDS)
                    && e.timestamp
                        <= report.generated_at + chrono::Duration::seconds(TIMESTAMP_JITTER_SECONDS)
                {
                    output = output.saturating_add(e.usage.output);
                    *agent_output.entry(e.id.agent).or_default() += e.usage.output;
                    let tokens = session_output.entry(e.session.clone()).or_default();
                    *tokens = tokens.saturating_add(e.usage.output);
                } else {
                    late += 1;
                }
            }
        }
        // Remember every identified record, including excluded ones.
        self.seen.extend(scan.observed_event_ids.iter().cloned());
        let source_agents: BTreeSet<AgentKind> =
            scan.sources.iter().map(|source| source.agent).collect();
        for agent in &source_agents {
            agent_totals.entry(*agent).or_default();
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
            self.output_speed_history.clear();
            self.chart_speed = Speed::default();
            self.chart_started = self.last_sample + self.chart_offset;
            for chart in self.agent_charts.values_mut() {
                chart.speed = Speed::default();
                chart.started = self.last_sample + chart.offset;
                chart.history.clear();
            }
        } else {
            self.delta += report.totals.total as i128 - self.previous_total as i128;
            self.cost_delta = self.cost_delta.and_then(|delta| {
                delta.checked_add(current.checked_sub(self.previous_cost.unwrap_or_default())?)
            });
        }
        let sample_seconds = now.saturating_sub(self.last_sample).as_secs_f64();
        // Independent token deltas and rates per agent.
        for (agent, total) in &agent_totals {
            let delta = if rollover || !self.agent_totals.contains_key(agent) {
                *total as i128
            } else {
                *total as i128 - self.agent_totals[agent] as i128
            };
            let since_ready = self.agent_since_ready.entry(*agent).or_default();
            if rollover {
                *since_ready = *total as i128;
            } else {
                *since_ready += delta;
            }
            self.agent_window.insert(*agent, delta);
            self.agent_rate.insert(
                *agent,
                if sample_seconds > 0.0 {
                    agent_output.get(agent).copied().unwrap_or_default() as f64 / sample_seconds
                } else {
                    0.0
                },
            );
        }
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
        let chart_rate = if self.has_sources && self.readable.is_empty() {
            // Do not average an outage as measured idle time or bridge it on recovery.
            self.chart_speed = Speed::default();
            self.chart_started = now + self.chart_offset;
            None
        } else {
            let elapsed = (now + self.chart_offset).saturating_sub(self.chart_started);
            self.chart_speed.record(elapsed, output);
            Some(
                self.chart_speed
                    .rate_over(elapsed, Duration::from_secs(CHART_SPEED_WINDOW_SECONDS)),
            )
        };
        self.output_speed_history.push_back(chart_rate);
        while self.output_speed_history.len() > SPEED_HISTORY_CAPACITY {
            self.output_speed_history.pop_front();
        }
        // Per-agent charts never borrow another agent's activity.
        for agent in &source_agents {
            let has_sources = scan.sources.iter().any(|source| source.agent == *agent);
            let available = !has_sources || readable_agent.contains(agent);
            let chart = self
                .agent_charts
                .entry(*agent)
                .or_insert_with(|| AgentChart {
                    offset: self.chart_offset,
                    ..Default::default()
                });
            let rate = if !available {
                chart.speed = Speed::default();
                chart.started = now + chart.offset;
                None
            } else {
                let elapsed = (now + chart.offset).saturating_sub(chart.started);
                chart.speed.record(
                    elapsed,
                    agent_output.get(agent).copied().unwrap_or_default(),
                );
                Some(
                    chart
                        .speed
                        .rate_over(elapsed, Duration::from_secs(CHART_SPEED_WINDOW_SECONDS)),
                )
            };
            chart.history.push_back(rate);
            while chart.history.len() > SPEED_HISTORY_CAPACITY {
                chart.history.pop_front();
            }
        }
        self.agent_totals = agent_totals;
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
        Cli::parse_from(["agent-usage", "monitor", "--interval", interval])
    }
    fn scan(readable: bool) -> Scan {
        Scan {
            sources: vec![SourceStatus {
                name: "local".into(),
                agent: AgentKind::Codex,
                root: "/not-read".into(),
                readable,
                last_success: None,
                error: None,
                capability: crate::model::QuotaCapability::RecordedSnapshots,
                coverage: Default::default(),
                pricing_provider: None,
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
            id: EventKey::new(AgentKind::Codex, id),
            timestamp: time,
            session: SessionKey::main(AgentKind::Codex, "s"),
            model: "test".into(),
            provider: "openai".into(),
            tier: Some("standard".into()),
            mode: crate::model::UsageMode::Regular,
            project: None,
            usage: Usage {
                input: 10,
                output,
                ..Default::default()
            },
            cache_writes: None,
            source: "local".into(),
            origins: BTreeSet::new(),
            context_tokens: None,
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
            assert!(startup.output_speed_history.is_empty());
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
            assert_eq!(
                info.output_speed_history.len(),
                3,
                "history counts observations, not seconds: {value}"
            );
            assert_eq!(
                info.output_speed_history[2],
                Some(120.0 / (actual * 3).as_secs_f64())
            );
        }
    }

    #[test]
    fn historical_chart_uses_filtered_windows_and_continues_without_recounting() {
        let cli = Cli::parse_from(["agent-usage", "monitor", "--model", "test"]);
        let mut scan = scan(true);
        let mut r = report("day1", 0, None);
        r.generated_at = "2026-11-04T12:00:00Z".parse().unwrap();
        r.period.start = Some("2026-11-04T00:00:00Z".parse().unwrap());
        let base = r.generated_at;
        for (second, output) in [
            (-149, 6000),
            (-148, 6000),
            (-120, 120),
            (-30, 300),
            (-10, 60),
            (1, 9000),
        ] {
            scan.events.push(event(
                &second.to_string(),
                output,
                base + chrono::Duration::seconds(second),
            ));
        }
        let mut excluded = event("other-model", 9000, base);
        excluded.model = "other".into();
        scan.events.push(excluded);
        scan.events.reverse(); // Logs and sources need not arrive in timestamp order.
        let mut tracker = Tracker::new(&scan, &r, false);
        tracker.seed_chart(&cli, &scan, &r);
        let info = tracker.info_at(0, Duration::ZERO);
        assert_eq!(info.output_speed_history.len(), 60);
        assert_eq!(info.output_speed_history.first(), Some(&Some(4.0)));
        assert_eq!(info.output_speed_history.last(), Some(&Some(2.0)));
        assert_eq!(info.window_tokens, 0);
        assert_eq!(info.output_tokens_per_second, 0.0);
        assert_eq!(info.average_session_output_tokens_per_second, 0.0);
        for (second, expected) in [(2, 3.0), (20, 1.0), (32, 0.0)] {
            r.generated_at = base + chrono::Duration::seconds(second);
            if second == 2 {
                scan.events.push(event("live", 30, r.generated_at));
                r.totals.total = 40;
            }
            tracker.observe_at(&cli, &scan, &mut r, 0, Duration::from_secs(second as u64));
            let info = r.monitor.as_ref().unwrap();
            assert_eq!(info.output_speed_history.len(), 60);
            assert_eq!(info.output_speed_history.last(), Some(&Some(expected)));
            assert_eq!(info.since_ready_tokens, 40);
            if second == 2 {
                assert_eq!(info.output_tokens_per_second, 15.0);
            }
        }
        r.period.label = "day2".into();
        r.generated_at = base + chrono::Duration::seconds(34);
        scan.events.push(event("new-day", 20, r.generated_at));
        tracker.observe_at(&cli, &scan, &mut r, 0, Duration::from_secs(34));
        assert_eq!(
            r.monitor.as_ref().unwrap().output_speed_history,
            vec![Some(10.0)]
        );
    }

    #[test]
    fn historical_chart_stops_at_local_midnight_and_marks_unreadable_sources() {
        let cli = cli("2s");
        let mut scan = scan(true);
        let mut r = report("day1", 0, None);
        // Local midnight for Asia/Shanghai is the previous UTC date at16:00.
        let midnight: chrono::DateTime<Utc> = "2026-11-03T16:00:00Z".parse().unwrap();
        r.period.start = Some(midnight);
        r.generated_at = midnight + chrono::Duration::seconds(5);
        for (second, output) in [(-1, 9999), (0, 100), (3, 30)] {
            scan.events.push(event(
                &second.to_string(),
                output,
                midnight + chrono::Duration::seconds(second),
            ));
        }
        let mut tracker = Tracker::new(&scan, &r, false);
        tracker.seed_chart(&cli, &scan, &r);
        assert_eq!(
            tracker.info_at(0, Duration::ZERO).output_speed_history,
            vec![Some(100.0), Some(130.0 / 3.0), Some(26.0)]
        );
        scan.sources[0].readable = false;
        let mut tracker = Tracker::new(&scan, &r, false);
        tracker.seed_chart(&cli, &scan, &r);
        assert_eq!(
            tracker.info_at(0, Duration::ZERO).output_speed_history,
            vec![None; 3]
        );
        scan.sources[0].readable = true;
        scan.events.clear();
        let mut tracker = Tracker::new(&scan, &r, false);
        tracker.seed_chart(&cli, &scan, &r);
        assert_eq!(
            tracker.info_at(0, Duration::ZERO).output_speed_history,
            vec![Some(0.0); 3]
        );
    }

    #[test]
    fn chart_smooths_parallel_bursts_and_expires_at_thirty_seconds_independently_of_sessions() {
        let mut scan = scan(true);
        let mut r = report("day1", 0, None);
        let base = r.generated_at;
        let mut tracker = Tracker::new(&scan, &r, false);
        for (second, outputs, expected) in [
            (2, vec![120, 60], 90.0),
            (4, vec![], 45.0),
            (10, vec![120], 30.0),
            (30, vec![], 10.0),
            (31, vec![], 10.0),
            (32, vec![], 4.0),
            (39, vec![], 4.0),
            (40, vec![], 0.0),
        ] {
            r.generated_at = base + chrono::Duration::seconds(second);
            for (index, tokens) in outputs.iter().enumerate() {
                let mut e = event(&format!("{second}-{index}"), *tokens, r.generated_at);
                e.session = SessionKey::main(AgentKind::Codex, format!("session-{index}"));
                scan.events.push(e);
            }
            tracker.observe_at(
                &cli("2s"),
                &scan,
                &mut r,
                0,
                Duration::from_secs(second as u64),
            );
            let info = r.monitor.as_ref().unwrap();
            assert_eq!(
                info.output_speed_history.last(),
                Some(&Some(expected)),
                "at {second}s"
            );
            if outputs.is_empty() {
                assert_eq!(
                    info.output_tokens_per_second, 0.0,
                    "raw rate stays immediate"
                );
            }
            if second == 2 {
                assert_eq!(info.average_session_output_tokens_per_second, 45.0);
            }
            assert_eq!(info.output_speed_window_seconds, 30);
            assert_eq!(info.session_speed_window_seconds, 60);
            if second == 40 {
                assert!(info.average_session_output_tokens_per_second > 0.0);
            }
        }
    }

    #[test]
    fn speed_history_records_refreshes_idle_and_unavailable_gaps_but_not_info_calls() {
        let mut scan = scan(true);
        let mut r = report("day1", 0, None);
        let base = r.generated_at;
        let mut tracker = Tracker::new(&scan, &r, false);
        assert!(tracker.info(0).output_speed_history.is_empty());
        r.generated_at = base + chrono::Duration::seconds(2);
        scan.events.push(event("fresh", 40, r.generated_at));
        r.totals.total = 50;
        tracker.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(2));
        assert_eq!(
            r.monitor.as_ref().unwrap().output_speed_history,
            vec![Some(20.0)]
        );
        r.generated_at = base + chrono::Duration::seconds(4);
        tracker.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(4));
        assert_eq!(
            r.monitor.as_ref().unwrap().output_speed_history,
            vec![Some(20.0), Some(10.0)]
        );
        scan.sources[0].readable = false;
        r.generated_at = base + chrono::Duration::seconds(6);
        tracker.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(6));
        assert_eq!(
            r.monitor.as_ref().unwrap().output_speed_history,
            vec![Some(20.0), Some(10.0), None]
        );
        scan.sources[0].readable = true;
        r.generated_at = base + chrono::Duration::seconds(8);
        scan.events.push(event("recovered", 30, r.generated_at));
        r.totals.total += 40;
        tracker.observe_at(&cli("2s"), &scan, &mut r, 0, Duration::from_secs(8));
        let expected = vec![Some(20.0), Some(10.0), None, Some(0.0)];
        assert_eq!(r.monitor.as_ref().unwrap().output_speed_history, expected);
        // Reading metadata/finalizing an already completed frame must not invent points.
        for second in [8, 9, 30] {
            assert_eq!(
                tracker
                    .info_at(0, Duration::from_secs(second))
                    .output_speed_history,
                expected
            );
        }
    }

    #[test]
    fn speed_history_truncates_oldest_points_and_resets_before_new_day_point() {
        let mut scan = scan(true);
        let mut r = report("day1", 0, None);
        let base = r.generated_at;
        let mut tracker = Tracker::new(&scan, &r, false);
        for second in 1..=65 {
            r.generated_at = base + chrono::Duration::seconds(second);
            scan.events
                .push(event(&format!("r-{second}"), second as u64, r.generated_at));
            r.totals.total += 10 + second as u64;
            tracker.observe_at(
                &cli("1s"),
                &scan,
                &mut r,
                0,
                Duration::from_secs(second as u64),
            );
        }
        let history = &r.monitor.as_ref().unwrap().output_speed_history;
        assert_eq!(history.len(), SPEED_HISTORY_CAPACITY);
        assert_eq!(history.first(), Some(&Some(3.5)));
        assert_eq!(history.last(), Some(&Some(50.5)));
        let mut next_day = report("day2", 30, None);
        next_day.generated_at = base + chrono::Duration::seconds(66);
        scan.events
            .push(event("new-day", 20, next_day.generated_at));
        tracker.observe_at(&cli("1s"), &scan, &mut next_day, 0, Duration::from_secs(66));
        assert_eq!(
            next_day.monitor.as_ref().unwrap().output_speed_history,
            vec![Some(20.0)]
        );
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
            .entry(SessionKey::main(AgentKind::Codex, "yesterday"))
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
    fn codex_and_claude_activity_and_charts_are_measured_separately() {
        let cli = cli("2s");
        let status = |name: &str, agent: AgentKind| SourceStatus {
            name: name.into(),
            agent,
            root: "/not-read".into(),
            readable: true,
            last_success: None,
            error: None,
            capability: crate::model::QuotaCapability::Unsupported,
            coverage: Default::default(),
            pricing_provider: None,
        };
        let mut scan = Scan {
            sources: vec![
                status("codex-root", AgentKind::Codex),
                status("claude-root", AgentKind::ClaudeCode),
            ],
            ..Default::default()
        };
        let mut r = report("day1", 0, None);
        let mut tracker = Tracker::new(&scan, &r, false);
        let now = r.generated_at;
        let mut codex = event("c", 5, now);
        codex.source = "codex-root".into();
        let mut claude = event("d", 7, now);
        claude.id = EventKey::new(AgentKind::ClaudeCode, "d");
        claude.session = SessionKey::main(AgentKind::ClaudeCode, "claude-session");
        claude.model = "deepseek-flash".into();
        claude.provider = "unknown".into();
        claude.source = "claude-root".into();
        scan.events = vec![codex, claude];
        tracker.observe_at(&cli, &scan, &mut r, 0, Duration::from_secs(2));
        let info = r.monitor.as_ref().unwrap();
        assert_eq!(info.agent_activity.len(), 2);
        let codex = info
            .agent_activity
            .iter()
            .find(|agent| agent.agent == AgentKind::Codex)
            .unwrap();
        let claude = info
            .agent_activity
            .iter()
            .find(|agent| agent.agent == AgentKind::ClaudeCode)
            .unwrap();
        assert_eq!(codex.window_tokens, 15);
        assert_eq!(claude.window_tokens, 17);
        assert_eq!(codex.output_tokens_per_second, 2.5);
        assert_eq!(claude.output_tokens_per_second, 3.5);
        assert_eq!(codex.output_speed_history.last(), Some(&Some(2.5)));
        assert_eq!(claude.output_speed_history.last(), Some(&Some(3.5)));
        // A later Codex-only burst leaves Claude's chart at zero, not Codex's rate.
        r.generated_at = now + chrono::Duration::seconds(2);
        let mut more = event("e", 10, r.generated_at);
        more.source = "codex-root".into();
        scan.events.push(more);
        tracker.observe_at(&cli, &scan, &mut r, 0, Duration::from_secs(4));
        let info = r.monitor.as_ref().unwrap();
        let claude = info
            .agent_activity
            .iter()
            .find(|agent| agent.agent == AgentKind::ClaudeCode)
            .unwrap();
        assert_eq!(claude.window_tokens, 0);
        assert_eq!(claude.output_tokens_per_second, 0.0);
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
            .entry(SessionKey::main(AgentKind::Codex, "a"))
            .or_default()
            .record(Duration::from_secs(1), 300);
        tracker
            .session_speeds
            .entry(SessionKey::main(AgentKind::Codex, "b"))
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
                basis: "calendar".into(),
                label: period.into(),
                timezone: "UTC".into(),
                start: None,
                end: None,
            },
            rows: Vec::new(),
            subtotals: Vec::new(),
            totals: Counts {
                total,
                cost_usd: cost,
                ..Default::default()
            },
            sources: Vec::new(),
            coverage: Coverage::default(),
            pricing: serde_json::json!({}),
            quotas: Vec::new(),
            monitor: None,
        }
    }
    #[test]
    fn empty_start_and_empty_midnight_keep_cost_delta_available() {
        let cli = Cli::parse_from(["agent-usage", "monitor"]);
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
            .entry(SessionKey::main(AgentKind::Codex, "yesterday"))
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
        let cli = Cli::parse_from(["agent-usage", "monitor", "--no-cost"]);
        let event = UsageEvent {
            id: EventKey::new(AgentKind::Codex, "request"),
            timestamp: Utc::now(),
            session: SessionKey::main(AgentKind::Codex, "s"),
            model: "test".into(),
            provider: "openai".into(),
            tier: None,
            mode: crate::model::UsageMode::Unknown,
            project: None,
            usage: Usage {
                input: 10,
                output: 5,
                ..Default::default()
            },
            cache_writes: None,
            source: "local".into(),
            origins: BTreeSet::new(),
            context_tokens: None,
        };
        let status = SourceStatus {
            name: "local".into(),
            agent: AgentKind::Codex,
            root: "/not-read".into(),
            readable: false,
            last_success: None,
            error: None,
            capability: crate::model::QuotaCapability::RecordedSnapshots,
            coverage: Default::default(),
            pricing_provider: None,
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
        fresh.id = EventKey::new(AgentKind::Codex, "fresh");
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
