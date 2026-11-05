//! Quota-aware reports use only metadata in the normal, bounded session scan.
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::{
    fs::{self, FileTimes},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    config: PathBuf,
    now: DateTime<Utc>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().join("logs");
        fs::create_dir_all(root.join("sessions")).unwrap();
        let config = dir.path().join("config.toml");
        Self {
            _dir: dir,
            root,
            config,
            now: DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap(),
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_codex-usage"))
            .arg("--config")
            .arg(&self.config)
            .arg("--root")
            .arg(&self.root)
            .args(["--timezone", "UTC", "--no-cost"])
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.extend(["--format", "json"]);
        let output = self.run(&args);
        assert!(
            output.status.success(),
            "status={:?}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn write(&self, name: &str, records: &[Value]) -> PathBuf {
        write_log(&self.root, name, records)
    }
    fn forecast_records(&self) -> Vec<Value> {
        let first = self.now - Duration::minutes(10);
        let last = self.now - Duration::seconds(30);
        let reset = self.now + Duration::hours(2);
        let mut records = metadata("quota-session", first);
        records.extend([
            quota(first, 60.0, 300, reset),
            usage("first", first + Duration::seconds(30), 1000, 100),
            quota(self.now - Duration::minutes(5), 65.0, 300, reset),
            usage("last", last - Duration::seconds(30), 1000, 100),
            quota(last, 70.0, 300, reset),
        ]);
        records
    }
}
fn iso(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}
fn write_log(root: &Path, name: &str, records: &[Value]) -> PathBuf {
    fs::create_dir_all(root.join("sessions")).unwrap();
    let path = root.join("sessions").join(format!("{name}.jsonl"));
    let mut file = fs::File::create(&path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
    path
}
fn metadata(session: &str, time: DateTime<Utc>) -> Vec<Value> {
    vec![
        json!({"type":"session_meta","timestamp":iso(time),"payload":{
            "id":session,"model_provider":"openai","cwd":"/projects/quota"}}),
        json!({"type":"turn_context","timestamp":iso(time),"payload":{
            "turn_id":"quota-turn","model":"gpt-6.1-sol","service_tier":"standard"}}),
    ]
}
fn usage(id: &str, time: DateTime<Utc>, input: u64, output: u64) -> Value {
    json!({"type":"token_usage_record","timestamp":iso(time),"payload":{
        "response_id":id,"session_id":"quota-session","thread_id":"quota-session",
        "turn_id":"quota-turn","usage":{"input_tokens":input,"cached_input_tokens":input/2,
        "cache_write_input_tokens":0,"output_tokens":output,"reasoning_output_tokens":output/2,
        "total_tokens":input+output}}})
}
fn quota(time: DateTime<Utc>, used: f64, minutes: u64, reset: DateTime<Utc>) -> Value {
    json!({"type":"event_msg","timestamp":iso(time),"payload":{
        "type":"token_count","info":null,"rate_limits":{"limit_id":"codex",
            "primary":{"used_percent":used,"window_minutes":minutes,"resets_at":reset.timestamp()}}}})
}
fn midnight(time: DateTime<Utc>) -> String {
    format!("{}T00:00:00Z", time.date_naive())
}

#[test]
fn bare_and_daily_start_at_exact_most_recent_reset_with_short_term_precedence() {
    let f = Fixture::new();
    let reset = f.now - Duration::hours(2);
    let mut records = metadata("quota-session", reset - Duration::hours(1));
    records.extend([
        usage("before", reset - Duration::seconds(1), 100, 10),
        usage("at", reset, 200, 20),
        usage("after", reset + Duration::seconds(1), 300, 30),
    ]);
    let mut snapshot = quota(
        f.now - Duration::seconds(30),
        20.0,
        300,
        reset + Duration::hours(5),
    );
    snapshot["payload"]["rate_limits"]["secondary"] = json!({
        "used_percent":80.0,"window_minutes":10080,
        "resets_at":(f.now+Duration::days(2)).timestamp()});
    records.push(snapshot);
    f.write("short-and-weekly", &records);
    for args in [vec![], vec!["daily"]] {
        let report = f.json(&args);
        assert_eq!(report["kind"], "Daily");
        assert_eq!(report["period"]["start"], iso(reset));
        assert_eq!(report["period"]["basis"], "last_reset_estimated");
        assert_eq!(report["totals"]["total"], 550);
        assert_eq!(report["totals"]["events"], 2);
        assert_eq!(report["quotas"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn explicit_date_ranges_override_quota_reset() {
    let f = Fixture::new();
    let reset = f.now - Duration::hours(2);
    let mut records = metadata("quota-session", reset);
    records.push(quota(
        f.now - Duration::seconds(30),
        20.0,
        300,
        reset + Duration::hours(5),
    ));
    f.write("quota", &records);
    let daily = f.json(&["daily", "--last", "7"]);
    assert_eq!(
        daily["period"]["start"],
        midnight(f.now - Duration::days(6))
    );
    assert_eq!(daily["period"]["basis"], "calendar");
    let today = f.json(&["daily", "--today"]);
    assert_eq!(today["period"]["start"], midnight(f.now));
    let since = f.json(&["daily", "--since", "2026-01-02", "--until", "2026-01-03"]);
    assert_eq!(since["period"]["start"], "2026-01-02T00:00:00Z");
    assert_eq!(since["period"]["end"], "2026-01-04T00:00:00Z");
    let all = f.json(&["daily", "--all"]);
    assert!(all["period"]["start"].is_null());
}

#[test]
fn a_previously_recorded_reset_deadline_is_preferred_to_an_inferred_boundary() {
    let f = Fixture::new();
    let reset = f.now - Duration::hours(1);
    let mut records = metadata("quota-session", reset - Duration::minutes(1));
    records.extend([
        quota(reset - Duration::minutes(1), 90.0, 300, reset),
        usage("at-reset", reset, 100, 10),
        quota(
            f.now - Duration::seconds(30),
            10.0,
            300,
            reset + Duration::hours(5),
        ),
    ]);
    f.write("reset-transition", &records);
    let report = f.json(&["daily"]);
    assert_eq!(report["period"]["basis"], "last_reset_observed");
    assert_eq!(report["period"]["start"], iso(reset));
    assert_eq!(report["quotas"][0]["last_reset_basis"], "observed");
    assert_eq!(report["totals"]["total"], 110);
}

#[test]
fn missing_malformed_and_expired_quota_use_seven_day_fallback() {
    for kind in ["missing", "malformed", "expired"] {
        let f = Fixture::new();
        let mut records = metadata("quota-session", f.now - Duration::hours(1));
        match kind {
            "malformed" => records.push(quota(
                f.now - Duration::seconds(30),
                -1.0,
                300,
                f.now + Duration::hours(1),
            )),
            "expired" => records.push(quota(
                f.now - Duration::hours(6),
                90.0,
                300,
                f.now - Duration::hours(1),
            )),
            _ => {}
        }
        f.write(kind, &records);
        let report = f.json(&["daily"]);
        assert_eq!(
            report["period"]["start"],
            midnight(f.now - Duration::days(6)),
            "{kind}: {report}"
        );
        assert_eq!(report["period"]["basis"], "last_7_days_fallback");
        if kind == "expired" {
            assert_eq!(report["quotas"][0]["status"], "expired");
            assert!(report["quotas"][0]["remaining_tokens"].is_null());
        } else {
            assert!(report.get("quotas").is_none());
        }
    }
}

#[test]
fn token_and_exhaustion_estimates_use_comparable_observations_without_double_counting() {
    let f = Fixture::new();
    let records = f.forecast_records();
    f.write("original", &records);
    let before = f.json(&["daily"]);
    assert_eq!(before["totals"]["total"], 2200);
    let estimate = &before["quotas"][0];
    assert_eq!(estimate["forecast_status"], "estimated", "{estimate}");
    assert_eq!(estimate["forecast_tokens"], 2200);
    assert_eq!(estimate["remaining_tokens"], 6600);
    let exhaustion: DateTime<Utc> = estimate["exhausts_at"].as_str().unwrap().parse().unwrap();
    assert!(exhaustion > f.now && exhaustion < f.now + Duration::hours(2));
    f.write("copied", &records);
    let copied = f.json(&["daily"]);
    assert_eq!(copied["totals"], before["totals"]);
    assert_eq!(copied["quotas"][0]["remaining_tokens"], 6600);
    assert_eq!(copied["quotas"][0]["forecast_tokens"], 2200);
    assert_eq!(copied["quotas"].as_array().unwrap().len(), 1);
}

#[test]
fn stale_quota_keeps_reset_context_but_has_no_forecast() {
    let f = Fixture::new();
    let first = f.now - Duration::minutes(30);
    let last = f.now - Duration::minutes(10);
    let reset = f.now + Duration::hours(2);
    let mut records = metadata("quota-session", first);
    records.extend([
        quota(first, 60.0, 300, reset),
        usage("first", first + Duration::seconds(30), 1000, 100),
        usage("last", last - Duration::seconds(30), 1000, 100),
        quota(last, 70.0, 300, reset),
    ]);
    f.write("stale", &records);
    let report = f.json(&["daily"]);
    let quota = &report["quotas"][0];
    assert_eq!(quota["status"], "stale");
    assert!(quota["remaining_tokens"].is_null());
    assert!(quota["exhausts_at"].is_null());
    assert_eq!(report["period"]["basis"], "last_reset_estimated");
}

#[test]
fn monitor_keeps_today_and_forecasts_from_its_existing_files() {
    let f = Fixture::new();
    f.write("forecast", &f.forecast_records());
    let output = f.run(&["monitor", "--frames", "1", "--format", "jsonl"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["kind"], "Monitor");
    assert_eq!(report["period"]["start"], midnight(f.now));
    assert_eq!(report["quotas"][0]["forecast_status"], "estimated");
    assert_eq!(report["quotas"][0]["remaining_tokens"], 6600);
}

#[test]
fn quota_metadata_does_not_expand_daily_or_monitor_file_search() {
    for (command, modified) in [
        ("daily", Duration::days(12)),
        ("monitor", Duration::days(3)),
    ] {
        let f = Fixture::new();
        let path = f.write("old", &f.forecast_records());
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified((f.now - modified).into()))
            .unwrap();
        let report = if command == "monitor" {
            let output = f.run(&["monitor", "--frames", "1", "--format", "jsonl"]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        } else {
            f.json(&["daily"])
        };
        assert!(report.get("quotas").is_none(), "{command}: {report}");
        assert_eq!(report["coverage"]["files"], 0);
        assert_eq!(report["totals"]["events"], 0);
    }
}

#[test]
fn conflicting_source_resets_are_not_combined_into_one_account() {
    let f = Fixture::new();
    let first_reset = f.now - Duration::hours(2);
    f.write(
        "quota",
        &[quota(
            f.now - Duration::seconds(30),
            20.0,
            300,
            first_reset + Duration::hours(5),
        )],
    );
    let other = f._dir.path().join("other");
    write_log(
        &other,
        "quota",
        &[quota(
            f.now - Duration::seconds(30),
            50.0,
            300,
            first_reset + Duration::hours(4),
        )],
    );
    let report = f.json(&[
        "daily",
        "--root",
        f.root.to_str().unwrap(),
        "--root",
        other.to_str().unwrap(),
    ]);
    assert_eq!(
        report["period"]["start"],
        midnight(f.now - Duration::days(6))
    );
    assert_eq!(report["period"]["basis"], "last_7_days_conflicting_resets");
    assert_eq!(report["quotas"].as_array().unwrap().len(), 2);
    assert_ne!(report["quotas"][0]["source"], report["quotas"][1]["source"]);
}
