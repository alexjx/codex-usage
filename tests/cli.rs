//! End-to-end contracts using only temporary synthetic session logs.
use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    str::FromStr,
};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().join("codex");
        fs::create_dir_all(root.join("sessions")).unwrap();
        let config = dir.path().join("settings/config.toml");
        Self {
            _dir: dir,
            root,
            config,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-usage"));
        command.arg("--config").arg(&self.config);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let output = self
            .command()
            .args(args)
            .arg("--root")
            .arg(&self.root)
            .args(["--all", "--no-cost", "--format", "json"])
            .output()
            .unwrap();
        success(&output);
        no_ansi(&output.stdout);
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn write(&self, name: &str, events: Vec<Value>) -> PathBuf {
        write_log(&self.root, name, events)
    }
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn no_ansi(output: &[u8]) {
    assert!(
        !output.contains(&0x1b),
        "machine-readable output contains ANSI escapes"
    );
}
fn write_log(root: &Path, name: &str, events: Vec<Value>) -> PathBuf {
    let dir = root.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{name}.jsonl"));
    let mut output = fs::File::create(&file).unwrap();
    for event in events {
        writeln!(output, "{event}").unwrap();
    }
    file
}
fn metadata(session: &str) -> Value {
    json!({"type":"session_meta","timestamp":"2026-11-01T00:00:00Z","payload":{
        "id":session,"session_id":session,"model_provider":"openai","cwd":"/projects/demo"}})
}
fn context(turn: &str, model: &str) -> Value {
    json!({"type":"turn_context","timestamp":"2026-11-01T00:00:00Z","payload":{
        "turn_id":turn,"model":model,"cwd":"/projects/demo","service_tier":"standard"}})
}
fn usage(
    session: &str,
    turn: &str,
    response: &str,
    time: &str,
    input: u64,
    cached: u64,
    output: u64,
) -> Value {
    json!({"type":"token_usage_record","timestamp":time,"payload":{
        "response_id":response,"session_id":session,"thread_id":session,"turn_id":turn,
        "usage":{"input_tokens":input,"cached_input_tokens":cached,"cache_write_input_tokens":0,
            "output_tokens":output,"reasoning_output_tokens":output / 2,"total_tokens":input + output}}})
}
fn legacy(time: &str, input: u64, output: u64) -> Value {
    json!({"type":"event_msg","timestamp":time,"payload":{"type":"token_count","info":{
        "total_token_usage":{"input_tokens":input,"cached_input_tokens":0,
            "output_tokens":output,"reasoning_output_tokens":0,"total_tokens":input + output}}}})
}
fn by_model<'a>(report: &'a Value, model: &str) -> &'a Value {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["dimensions"]["model"] == model)
        .unwrap_or_else(|| panic!("missing model {model} in {report}"))
}

#[test]
fn separate_source_configuration_persists_and_selects_only_requested_roots() {
    let f = Fixture::new();
    let windows = f._dir.path().join("windows");
    write_log(
        &f.root,
        "wsl",
        vec![
            metadata("wsl-session"),
            context("wsl-turn", "model-wsl"),
            usage(
                "wsl-session",
                "wsl-turn",
                "wsl-response",
                "2026-11-01T00:01:00Z",
                100,
                20,
                30,
            ),
        ],
    );
    write_log(
        &windows,
        "windows",
        vec![
            metadata("win-session"),
            context("win-turn", "model-win"),
            usage(
                "win-session",
                "win-turn",
                "win-response",
                "2026-11-01T00:01:00Z",
                200,
                50,
                40,
            ),
        ],
    );
    success(&f.run(&[
        "sources",
        "set",
        "wsl",
        "--kind",
        "wsl",
        "--root",
        f.root.to_str().unwrap(),
    ]));
    success(&f.run(&[
        "sources",
        "set",
        "windows",
        "--kind",
        "windows",
        "--root",
        windows.to_str().unwrap(),
    ]));
    success(&f.run(&["sources", "disable", "windows"]));
    let default = f.run(&["models", "--all", "--no-cost", "--format", "json"]);
    success(&default);
    let report: Value = serde_json::from_slice(&default.stdout).unwrap();
    assert_eq!(report["totals"]["total"], 130);
    assert_eq!(report["sources"].as_array().unwrap().len(), 1);
    let selected = f.run(&[
        "models",
        "--all",
        "--no-cost",
        "--format",
        "json",
        "--source",
        "windows",
    ]);
    success(&selected);
    let report: Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(report["totals"]["total"], 240);
    success(&f.run(&["sources", "enable", "windows"]));
    let combined = f.run(&["models", "--all", "--no-cost", "--format", "json"]);
    success(&combined);
    assert_eq!(
        serde_json::from_slice::<Value>(&combined.stdout).unwrap()["totals"]["total"],
        370
    );
    let saved: toml::Value = toml::from_str(&fs::read_to_string(&f.config).unwrap()).unwrap();
    assert_eq!(saved["sources"].as_array().unwrap().len(), 2);
    assert!(saved["sources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|source| source["enabled"].as_bool() == Some(true)));
    let ad_hoc = f.json(&["models"]);
    assert_eq!(ad_hoc["totals"]["total"], 130);
    assert_eq!(
        f.run(&[
            "models",
            "--source",
            "wsl",
            "--root",
            f.root.to_str().unwrap()
        ])
        .status
        .code(),
        Some(2)
    );
    assert!(!f.run(&["models", "--source", "missing"]).status.success());
}

#[test]
fn model_switches_unknown_metadata_and_distinct_sessions_reconcile() {
    let f = Fixture::new();
    f.write(
        "switch",
        vec![
            metadata("same-session"),
            context("first", "alpha"),
            usage(
                "same-session",
                "first",
                "request-1",
                "2026-11-01T00:01:00Z",
                100,
                60,
                20,
            ),
            context("second", "beta"),
            usage(
                "same-session",
                "second",
                "request-2",
                "2026-11-01T00:02:00Z",
                50,
                10,
                10,
            ),
        ],
    );
    f.write(
        "unknown",
        vec![
            metadata("unknown-session"),
            usage(
                "unknown-session",
                "unrecorded",
                "request-3",
                "2026-11-02T00:01:00Z",
                20,
                0,
                5,
            ),
        ],
    );
    let report = f.json(&["models", "--color", "always"]);
    assert_eq!(report["totals"]["input"], 170);
    assert_eq!(report["totals"]["cached"], 70);
    assert_eq!(report["totals"]["output"], 35);
    assert_eq!(report["totals"]["total"], 205);
    assert_eq!(report["totals"]["sessions"], 2);
    assert_eq!(by_model(&report, "alpha")["counts"]["total"], 120);
    assert_eq!(by_model(&report, "beta")["counts"]["total"], 60);
    assert_eq!(by_model(&report, "unknown")["counts"]["total"], 25);
    let sessions = f.json(&["sessions"]);
    assert_eq!(sessions["rows"].as_array().unwrap().len(), 3);
    let one = f.json(&["session", "same-session"]);
    assert_eq!(one["totals"]["total"], 180);
    assert_eq!(one["rows"].as_array().unwrap().len(), 2);
}

#[test]
fn daily_monthly_and_csv_export_preserve_all_usage_without_ansi() {
    let f = Fixture::new();
    f.write(
        "periods",
        vec![
            metadata("period-session"),
            context("turn", "alpha"),
            usage(
                "period-session",
                "turn",
                "period-1",
                "2026-10-31T23:59:00Z",
                100,
                60,
                20,
            ),
            usage(
                "period-session",
                "turn",
                "period-2",
                "2026-11-01T00:01:00Z",
                50,
                10,
                10,
            ),
        ],
    );
    let daily = f.json(&["daily", "--timezone", "UTC"]);
    assert_eq!(daily["rows"].as_array().unwrap().len(), 2);
    assert_eq!(daily["rows"][0]["dimensions"]["day"], "2026-10-31");
    assert_eq!(daily["rows"][1]["dimensions"]["day"], "2026-11-01");
    let monthly = f.json(&["monthly", "--timezone", "UTC"]);
    assert_eq!(monthly["rows"][0]["dimensions"]["month"], "2026-10");
    assert_eq!(monthly["rows"][1]["dimensions"]["month"], "2026-11");
    assert_eq!(monthly["totals"]["total"], 180);
    let output = f
        .command()
        .args([
            "daily",
            "--all",
            "--no-cost",
            "--format",
            "csv",
            "--color",
            "always",
            "--timezone",
            "UTC",
        ])
        .arg("--root")
        .arg(&f.root)
        .output()
        .unwrap();
    success(&output);
    no_ansi(&output.stdout);
    let mut csv = csv::Reader::from_reader(output.stdout.as_slice());
    let headers = csv.headers().unwrap().clone();
    let total_column = headers
        .iter()
        .position(|column| column == "total")
        .expect("CSV needs a flat total column");
    let records = csv.records().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!(records.len(), 2);
    let total = records
        .iter()
        .map(|row| row[total_column].parse::<u64>().unwrap())
        .sum::<u64>();
    assert_eq!(total, 180);
}

#[test]
fn timezone_date_bounds_are_inclusive_without_losing_prior_counter_baseline() {
    let f = Fixture::new();
    f.write(
        "legacy",
        vec![
            metadata("legacy-session"),
            context("legacy-turn", "legacy-model"),
            legacy("2026-10-31T15:59:59Z", 100, 20),
            legacy("2026-10-31T16:00:00Z", 150, 30),
            legacy("2026-11-01T15:59:59Z", 170, 35),
            legacy("2026-11-01T15:59:59Z", 170, 35),
            legacy("2026-11-01T16:00:00Z", 190, 40),
        ],
    );
    let output = f
        .command()
        .args([
            "daily",
            "--since",
            "2026-11-01",
            "--until",
            "2026-11-01",
            "--timezone",
            "Asia/Shanghai",
            "--no-cost",
            "--format",
            "json",
        ])
        .arg("--root")
        .arg(&f.root)
        .output()
        .unwrap();
    success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["period"]["timezone"], "Asia/Shanghai");
    assert_eq!(report["rows"].as_array().unwrap().len(), 1);
    assert_eq!(report["rows"][0]["dimensions"]["day"], "2026-11-01");
    assert_eq!(report["totals"]["input"], 70);
    assert_eq!(report["totals"]["output"], 15);
    assert_eq!(report["totals"]["total"], 85);
}

#[test]
fn copied_cross_source_sessions_are_counted_once_but_each_source_can_stand_alone() {
    let f = Fixture::new();
    let second = f._dir.path().join("copy");
    let records = vec![
        metadata("copied-session"),
        context("copy-turn", "alpha"),
        usage(
            "copied-session",
            "copy-turn",
            "same-response",
            "2026-11-01T00:01:00Z",
            100,
            60,
            20,
        ),
    ];
    f.write("original", records.clone());
    write_log(&second, "copy", records);
    let output = f
        .command()
        .args(["models", "--all", "--no-cost", "--format", "json"])
        .arg("--root")
        .arg(&f.root)
        .arg("--root")
        .arg(&second)
        .output()
        .unwrap();
    success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["total"], 120);
    assert_eq!(report["totals"]["sessions"], 1);
    assert_eq!(report["sources"].as_array().unwrap().len(), 2);
    assert!(report["coverage"]["duplicates"].as_u64().unwrap() > 0);
    assert_eq!(f.json(&["models"])["totals"]["total"], 120);
}

#[test]
fn custom_model_cost_is_exact_and_unknown_cost_never_becomes_zero() {
    let f = Fixture::new();
    f.write(
        "cost",
        vec![
            metadata("cost-session"),
            context("priced", "my-model"),
            usage(
                "cost-session",
                "priced",
                "priced-response",
                "2026-11-01T00:01:00Z",
                100_000,
                60_000,
                20_000,
            ),
            context("unknown", "unpriced-model"),
            usage(
                "cost-session",
                "unknown",
                "unpriced-response",
                "2026-11-01T00:02:00Z",
                2000,
                0,
                500,
            ),
        ],
    );
    let rates = f._dir.path().join("rates.toml");
    fs::write(&rates,"[[rates]]\nmodel='my-model'\nprovider='openai'\ntier='standard'\ninput='3'\ncached='1'\noutput='10'\n").unwrap();
    let output = f
        .command()
        .args(["models", "--all", "--format", "json"])
        .arg("--root")
        .arg(&f.root)
        .arg("--pricing-file")
        .arg(&rates)
        .output()
        .unwrap();
    success(&output);
    no_ansi(&output.stdout);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let cost = by_model(&report, "my-model")["counts"]["cost_usd"]
        .as_str()
        .unwrap();
    // 40k ordinary * $3/M + 60k cached * $1/M + 20k output * $10/M = $0.38.
    assert_eq!(
        Decimal::from_str(cost).unwrap(),
        Decimal::from_str("0.38").unwrap()
    );
    assert!(by_model(&report, "unpriced-model")["counts"]["cost_usd"].is_null());
    assert_eq!(report["totals"]["priced_tokens"], 120_000);
    assert_eq!(report["totals"]["unpriced_tokens"], 2500);
    assert_eq!(report["totals"]["pricing_status"], "partial");
}

#[test]
fn reports_and_monitor_filter_old_files_with_a_two_day_modification_buffer() {
    let f = Fixture::new();
    let now = Utc::now();
    let resumed = f.write(
        "rollout-2026-01-01-resumed",
        vec![
            metadata("resumed"),
            context("t", "alpha"),
            usage("resumed", "t", "r", &now.to_rfc3339(), 20, 0, 5),
        ],
    );
    // Even a retained modification time from yesterday is inside the buffer.
    fs::File::options()
        .write(true)
        .open(resumed)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified((now - chrono::Duration::days(1)).into()))
        .unwrap();
    let archive = f.root.join("sessions/old.jsonl.zst");
    fs::write(&archive, b"must not be decompressed for a recent report").unwrap();
    fs::File::options()
        .write(true)
        .open(archive)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified((now - chrono::Duration::days(4)).into()))
        .unwrap();
    for args in [
        vec!["models", "--today", "--format", "json"],
        vec!["daily", "--last", "1", "--format", "json"],
        vec!["monitor", "--frames", "1", "--format", "jsonl"],
    ] {
        let output = f
            .command()
            .args(args)
            .arg("--root")
            .arg(&f.root)
            .args(["--timezone", "UTC", "--no-cost", "--strict"])
            .output()
            .unwrap();
        success(&output);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["totals"]["total"], 25);
        assert_eq!(report["coverage"]["files"], 1);
    }
    // All-time reports still inspect the archive and expose its error.
    let all = f
        .command()
        .args(["models", "--all", "--no-cost", "--strict"])
        .arg("--root")
        .arg(&f.root)
        .output()
        .unwrap();
    assert!(!all.status.success());
}

#[test]
fn monitor_reads_real_appends_and_defers_partial_records_without_counting_history_as_speed() {
    let f = Fixture::new();
    let file = f.write(
        "live",
        vec![
            metadata("live-session"),
            context("live-turn", "alpha"),
            usage(
                "live-session",
                "live-turn",
                "old-response",
                &Utc::now().to_rfc3339(),
                100,
                20,
                30,
            ),
        ],
    );
    let mut child = f
        .command()
        .args([
            "monitor",
            "--no-cost",
            "--format",
            "jsonl",
            "--frames",
            "3",
            "--interval",
            "1s",
        ])
        .arg("--root")
        .arg(&f.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let baseline: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(baseline["totals"]["total"], 130);
    assert_eq!(baseline["monitor"]["output_tokens_per_second"], 0.0);
    assert_eq!(
        baseline["monitor"]["average_session_output_tokens_per_second"],
        0.0
    );
    assert_eq!(
        baseline["monitor"]["session_output_tokens_per_second"],
        json!({})
    );
    let append = usage(
        "live-session",
        "live-turn",
        "new-response",
        &Utc::now().to_rfc3339(),
        20,
        0,
        5,
    )
    .to_string();
    let middle = append.len() / 2;
    let mut writer = OpenOptions::new().append(true).open(&file).unwrap();
    writer.write_all(&append.as_bytes()[..middle]).unwrap();
    writer.flush().unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let partial: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(partial["totals"]["total"], 130);
    assert!(partial["coverage"]["pending"].as_u64().unwrap() > 0);
    assert_eq!(
        partial["monitor"]["average_session_output_tokens_per_second"],
        0.0
    );
    writer.write_all(&append.as_bytes()[middle..]).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
    let mut snapshots = vec![baseline, partial];
    for line in reader.lines() {
        let line = line.unwrap();
        no_ansi(line.as_bytes());
        snapshots.push(serde_json::from_str(&line).unwrap());
    }
    let output = child.wait_with_output().unwrap();
    success(&output);
    let last = snapshots.last().unwrap();
    assert_eq!(last["totals"]["total"], 155);
    assert_eq!(last["monitor"]["since_ready_tokens"], 25);
    let monitor = &last["monitor"];
    assert_eq!(monitor["session_speed_window_seconds"], 30);
    assert!(
        monitor["average_session_output_tokens_per_second"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert_eq!(
        monitor["average_session_output_tokens_per_second"],
        monitor["session_output_tokens_per_second"]["live-session"]
    );
    assert!(snapshots.iter().skip(2).any(|snapshot| snapshot["monitor"]
        ["output_tokens_per_second"]
        .as_f64()
        .unwrap()
        > 0.0));
    assert_eq!(f.json(&["models"])["totals"]["total"], 155);
}

fn recorded_mode(time: &str, tier: Option<&str>) -> Value {
    // Codex persists thread settings inside payload.thread_settings, not just turn_context.
    json!({"type":"thread_settings_applied","timestamp":time,
        "payload":{"thread_settings":{"service_tier":tier}}})
}
fn mode_context(turn: &str, model: &str, time: &str) -> Value {
    let mut value = context(turn, model);
    value["timestamp"] = json!(time);
    value["payload"]
        .as_object_mut()
        .unwrap()
        .remove("service_tier");
    value
}
fn dated_metadata(session: &str, time: &str) -> Value {
    let mut value = metadata(session);
    value["timestamp"] = json!(time);
    value
}
fn mode_fixture(
    f: &Fixture,
    input: u64,
    cached: u64,
    output: u64,
    include_unknown: bool,
    second_day: bool,
) {
    let mut events = vec![
        dated_metadata("mode-session", "2026-10-10T08:00:00Z"),
        mode_context("regular-turn", "gpt-6.1-sol", "2026-10-10T08:00:00Z"),
        recorded_mode("2026-10-10T08:00:00Z", Some("standard")),
        usage(
            "mode-session",
            "regular-turn",
            "regular-request",
            "2026-10-10T08:01:00Z",
            input,
            cached,
            output,
        ),
        mode_context("fast-turn", "gpt-6.1-sol", "2026-10-10T08:02:00Z"),
        recorded_mode("2026-10-10T08:02:00Z", Some("priority")),
        usage(
            "mode-session",
            "fast-turn",
            "fast-request",
            "2026-10-10T08:03:00Z",
            input,
            cached,
            output,
        ),
    ];
    if second_day {
        events.extend([
            mode_context("regular-next", "gpt-6.1-sol", "2026-10-11T08:00:00Z"),
            recorded_mode("2026-10-11T08:00:00Z", Some("default")),
            usage(
                "mode-session",
                "regular-next",
                "regular-next-request",
                "2026-10-11T08:01:00Z",
                input,
                cached,
                output,
            ),
            mode_context("fast-next", "gpt-6.1-sol", "2026-10-11T08:02:00Z"),
            recorded_mode("2026-10-11T08:02:00Z", Some("fast")),
            usage(
                "mode-session",
                "fast-next",
                "fast-next-request",
                "2026-10-11T08:03:00Z",
                input,
                cached,
                output,
            ),
        ]);
    }
    f.write("modes", events);
    if include_unknown {
        f.write(
            "unknown-mode",
            vec![
                dated_metadata("unknown-mode-session", "2026-10-10T09:00:00Z"),
                mode_context("unknown-mode-turn", "gpt-6.1-sol", "2026-10-10T09:00:00Z"),
                usage(
                    "unknown-mode-session",
                    "unknown-mode-turn",
                    "unknown-mode-request",
                    "2026-10-10T09:01:00Z",
                    input,
                    cached,
                    output,
                ),
            ],
        );
    }
}
fn priced_json(f: &Fixture, args: &[&str]) -> Value {
    let output = f
        .command()
        .args(args)
        .arg("--root")
        .arg(&f.root)
        .args(["--all", "--format", "json"])
        .output()
        .unwrap();
    success(&output);
    no_ansi(&output.stdout);
    serde_json::from_slice(&output.stdout).unwrap()
}
fn by_mode<'a>(report: &'a Value, mode: &str) -> &'a Value {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["dimensions"]["mode"] == mode)
        .unwrap_or_else(|| panic!("missing mode {mode} in {report}"))
}

#[test]
fn all_event_dates_use_current_regular_and_fast_rate_estimates() {
    let f = Fixture::new();
    mode_fixture(&f, 100_000, 60_000, 20_000, false, false);
    let report = priced_json(&f, &["models"]);
    assert_eq!(report["rows"].as_array().unwrap().len(), 2);
    assert_eq!(
        by_mode(&report, "regular")["dimensions"]["model"],
        "gpt-6.1-sol"
    );
    assert_eq!(
        by_mode(&report, "fast")["dimensions"]["model"],
        "gpt-6.1-sol"
    );
    // Regular = 40k*$2/M + 60k*$0.10/M + 20k*$10/M = $0.286.
    // Published Fast rates are twice these component rates, so the same tokens cost $0.572.
    assert_eq!(
        Decimal::from_str(
            by_mode(&report, "regular")["counts"]["cost_usd"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        Decimal::from_str("0.286").unwrap()
    );
    assert_eq!(
        Decimal::from_str(
            by_mode(&report, "fast")["counts"]["cost_usd"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        Decimal::from_str("0.572").unwrap()
    );
    assert_eq!(
        Decimal::from_str(report["totals"]["cost_usd"].as_str().unwrap()).unwrap(),
        Decimal::from_str("0.858").unwrap()
    );
    assert_eq!(report["totals"]["total"], 240_000);
    // The selected tariff is current even for a retained record outside its verification date.
    // Future timestamps exercise the opposite date boundary without depending on wall-clock time.
    f.write(
        "future-record",
        vec![
            dated_metadata("future-session", "2050-01-01T00:00:00Z"),
            mode_context("future-turn", "gpt-6.1-sol", "2050-01-01T00:00:00Z"),
            recorded_mode("2050-01-01T00:00:00Z", Some("standard")),
            usage(
                "future-session",
                "future-turn",
                "future-request",
                "2050-01-01T00:01:00Z",
                100_000,
                60_000,
                20_000,
            ),
        ],
    );
    let with_future = priced_json(&f, &["models"]);
    assert_eq!(
        Decimal::from_str(
            by_mode(&with_future, "regular")["counts"]["cost_usd"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        Decimal::from_str("0.572").unwrap()
    );
    assert_eq!(
        Decimal::from_str(with_future["totals"]["cost_usd"].as_str().unwrap()).unwrap(),
        Decimal::from_str("1.144").unwrap()
    );
    assert_eq!(with_future["totals"]["total"], 360_000);
    assert_eq!(with_future["totals"]["unpriced_tokens"], 0);
}

#[test]
fn default_model_groups_separate_fast_regular_and_unknown_and_mode_filter_is_exact() {
    let f = Fixture::new();
    mode_fixture(&f, 100, 60, 20, true, false);
    let report = f.json(&["models"]);
    assert_eq!(report["rows"].as_array().unwrap().len(), 3);
    assert_eq!(report["totals"]["total"], 360);
    for mode in ["regular", "fast", "unknown"] {
        assert_eq!(by_mode(&report, mode)["counts"]["total"], 120);
        assert_eq!(by_mode(&report, mode)["dimensions"]["model"], "gpt-6.1-sol");
    }
    let fast = f.json(&["models", "--mode", "fast"]);
    assert_eq!(fast["rows"].as_array().unwrap().len(), 1);
    assert_eq!(fast["totals"]["total"], 120);
    assert_eq!(fast["rows"][0]["dimensions"]["mode"], "fast");
    let unknown = f.json(&["models", "--mode", "unknown"]);
    assert_eq!(unknown["totals"]["total"], 120);
    assert_eq!(unknown["rows"][0]["dimensions"]["mode"], "unknown");
    let output = f
        .command()
        .args([
            "models",
            "--all",
            "--no-cost",
            "--color",
            "never",
            "--pager",
            "never",
        ])
        .arg("--root")
        .arg(&f.root)
        .output()
        .unwrap();
    success(&output);
    let human = String::from_utf8(output.stdout).unwrap();
    assert!(human.contains("Mode"));
    assert!(human.contains("Fast"));
    assert!(human.contains("Regular"));
    assert!(human.contains("Unknown"));
}

#[test]
fn daily_human_dates_are_single_group_headings_while_exports_keep_day_and_mode_per_row() {
    let f = Fixture::new();
    mode_fixture(&f, 100, 60, 20, true, true);
    let output = f
        .command()
        .args([
            "daily",
            "--all",
            "--no-cost",
            "--color",
            "never",
            "--pager",
            "never",
            "--timezone",
            "UTC",
        ])
        .arg("--root")
        .arg(&f.root)
        .output()
        .unwrap();
    success(&output);
    let human = String::from_utf8(output.stdout).unwrap();
    assert_eq!(human.matches("Sat, Oct 10 2026").count(), 1, "{human}");
    assert_eq!(human.matches("Sun, Oct 11 2026").count(), 1, "{human}");
    assert!(
        !human.contains("2026-10-10 /"),
        "human rows should not repeat date prefixes: {human}"
    );
    assert!(
        !human.contains("2026-10-11 /"),
        "human rows should not repeat date prefixes: {human}"
    );
    assert!(human.contains("Fast") && human.contains("Regular") && human.contains("Unknown"));
    let report = f.json(&["daily", "--timezone", "UTC"]);
    assert_eq!(report["rows"].as_array().unwrap().len(), 5);
    assert_eq!(report["totals"]["total"], 600);
    for row in report["rows"].as_array().unwrap() {
        assert!(["2026-10-10", "2026-10-11"].contains(&row["dimensions"]["day"].as_str().unwrap()));
        assert!(
            ["fast", "regular", "unknown"].contains(&row["dimensions"]["mode"].as_str().unwrap())
        );
    }
    let csv = f
        .command()
        .args([
            "daily",
            "--all",
            "--no-cost",
            "--format",
            "csv",
            "--color",
            "always",
            "--timezone",
            "UTC",
        ])
        .arg("--root")
        .arg(&f.root)
        .output()
        .unwrap();
    success(&csv);
    no_ansi(&csv.stdout);
    let mut csv = csv::Reader::from_reader(csv.stdout.as_slice());
    let headers = csv.headers().unwrap().clone();
    let day = headers
        .iter()
        .position(|name| name == "day")
        .expect("CSV day column");
    let mode = headers
        .iter()
        .position(|name| name == "mode")
        .expect("CSV mode column");
    let total = headers
        .iter()
        .position(|name| name == "total")
        .expect("CSV total column");
    let records = csv.records().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!(records.len(), 5);
    assert_eq!(
        records
            .iter()
            .map(|record| record[total].parse::<u64>().unwrap())
            .sum::<u64>(),
        600
    );
    assert!(records
        .iter()
        .all(|record| ["2026-10-10", "2026-10-11"].contains(&record[day].as_ref())));
    assert_eq!(
        records
            .iter()
            .filter(|record| &record[mode] == "fast")
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| &record[mode] == "regular")
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| &record[mode] == "unknown")
            .count(),
        1
    );
}

#[test]
fn human_reports_end_with_full_selection_totals_and_partial_cost_markers() {
    let f = Fixture::new();
    f.write(
        "totals",
        vec![
            metadata("totals-session"),
            context("priced", "gpt-6.1-sol"),
            usage(
                "totals-session",
                "priced",
                "one",
                &Utc::now().to_rfc3339(),
                100_000,
                60_000,
                20_000,
            ),
            context("unknown", "unpriced-model"),
            usage(
                "totals-session",
                "unknown",
                "two",
                &Utc::now().to_rfc3339(),
                2_000,
                0,
                500,
            ),
        ],
    );
    for command in [
        "models", "daily", "monthly", "sessions", "session", "monitor",
    ] {
        let mut cmd = f.command();
        cmd.arg(command);
        if command == "session" {
            cmd.arg("totals-session");
        }
        if command == "monitor" {
            cmd.args(["--frames", "1"]);
        } else {
            cmd.arg("--all");
        }
        let output = cmd
            .arg("--root")
            .arg(&f.root)
            .args([
                "--exact", "--format", "table", "--color", "never", "--pager", "never",
            ])
            .output()
            .unwrap();
        success(&output);
        no_ansi(&output.stdout);
        let text = String::from_utf8(output.stdout).unwrap();
        let total = text
            .lines()
            .find(|line| line.starts_with("Total "))
            .unwrap_or_else(|| panic!("{command}: {text}"));
        // Cache is a subset, not an additional charge to total tokens.
        assert_eq!(text.lines().last(), Some(total));
        let cells: Vec<_> = total.split_whitespace().collect();
        assert_eq!(
            cells,
            vec!["Total", "102,000", "60,000", "20,500", "122,500", "$0.2860*"],
            "{command}: {text}"
        );
        assert!(text.rfind("\nTotal ").unwrap() > text.rfind("unpriced-model").unwrap());
        for label in ["Unpriced:", "Data warnings:", "Pricing assumptions:"] {
            assert!(
                !text.contains(label),
                "{command}: unexpected footer {label}"
            );
        }
    }
    let empty = Fixture::new();
    let output = empty
        .command()
        .arg("--root")
        .arg(&empty.root)
        .args(["--all", "--no-cost", "--color", "never", "--pager", "never"])
        .output()
        .unwrap();
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.lines()
            .find(|line| line.starts_with("Total "))
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>(),
        vec!["Total", "0", "0", "0", "0"]
    );
}

#[test]
fn monitor_summarizes_today_only_and_rejects_historical_ranges() {
    let f = Fixture::new();
    let now = Utc::now();
    f.write(
        "today-only",
        vec![
            metadata("today-session"),
            context("turn", "alpha"),
            usage(
                "today-session",
                "turn",
                "yesterday",
                &(now - chrono::Duration::days(1)).to_rfc3339(),
                100,
                0,
                30,
            ),
            usage(
                "today-session",
                "turn",
                "today",
                &now.to_rfc3339(),
                20,
                0,
                5,
            ),
            usage(
                "today-session",
                "turn",
                "tomorrow",
                &(now + chrono::Duration::days(1)).to_rfc3339(),
                400,
                0,
                50,
            ),
        ],
    );
    let output = f
        .command()
        .args(["monitor", "--frames", "1", "--no-cost", "--timezone", "UTC"])
        .arg("--root")
        .arg(&f.root)
        .output()
        .unwrap();
    success(&output);
    let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot["period"]["label"], now.date_naive().to_string());
    assert_eq!(snapshot["totals"]["total"], 25);
    assert_eq!(snapshot["totals"]["events"], 1);
    assert_eq!(snapshot["monitor"]["since_ready_tokens"], 0);
    assert_eq!(f.json(&["models"])["totals"]["total"], 605);
    for args in [
        vec!["monitor", "--all"],
        vec!["--all", "monitor"],
        vec!["monitor", "--since", "2026-11-01"],
        vec!["monitor", "--until", "2026-11-01"],
        vec!["monitor", "--last", "7"],
    ] {
        let output = f.run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("today only"));
    }
    let help = f.run(&["monitor", "--help"]);
    success(&help);
    let help = String::from_utf8(help.stdout).unwrap();
    for flag in ["--all", "--since", "--until", "--today", "--last"] {
        assert!(!help.contains(flag), "{help}");
    }
    let help = f.run(&["daily", "--help"]);
    success(&help);
    assert!(String::from_utf8(help.stdout).unwrap().contains("--since"));
}

#[test]
fn listed_implicit_native_source_can_be_enabled_and_disabled() {
    let f = Fixture::new();
    let listed = f
        .command()
        .env("CODEX_HOME", &f.root)
        .args(["sources", "--format", "json"])
        .output()
        .unwrap();
    success(&listed);
    let catalog: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let name = catalog["sources"][0]["name"].as_str().unwrap();
    let enabled = f
        .command()
        .env("CODEX_HOME", &f.root)
        .args(["sources", "enable", name])
        .output()
        .unwrap();
    success(&enabled);
    let config: toml::Value = toml::from_str(&fs::read_to_string(&f.config).unwrap()).unwrap();
    let sources = config["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["name"].as_str(), Some(name));
    assert_eq!(sources[0]["enabled"].as_bool(), Some(true));
    success(&f.run(&["sources", "disable", name]));
    let config: toml::Value = toml::from_str(&fs::read_to_string(&f.config).unwrap()).unwrap();
    assert_eq!(config["sources"][0]["enabled"].as_bool(), Some(false));
}
