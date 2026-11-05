//! Fresh invocation defaults and calendar ranges, verified through the real binary.
//! Every command uses isolated temporary configuration and synthetic logs.
use chrono::{Days, NaiveDate, Utc};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output},
};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    config: PathBuf,
    today: NaiveDate,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().join("logs");
        fs::create_dir_all(root.join("sessions")).unwrap();
        let config = dir.path().join("settings/config.toml");
        let today = Utc::now().date_naive();
        let old = today.checked_sub_days(Days::new(3)).unwrap();
        let excluded = today.checked_sub_days(Days::new(9)).unwrap();
        let start = today.checked_sub_days(Days::new(15)).unwrap();
        let mut log = fs::File::create(root.join("sessions/rollout-defaults-test.jsonl")).unwrap();
        writeln!(log, "{}", json!({"type":"session_meta", "timestamp":format!("{start}T00:00:00Z"),
            "payload":{"id":"defaults-session","session_id":"defaults-session","model_provider":"openai","cwd":"/projects/defaults"}})).unwrap();
        for (index, (date, model, input)) in [
            (today, "alpha", 100),
            (old, "alpha", 200),
            (today, "beta", 300),
            (old, "beta", 400),
            (excluded, "alpha", 500),
        ]
        .into_iter()
        .enumerate()
        {
            let time = format!("{date}T12:00:00Z");
            let turn = format!("turn-{index}");
            writeln!(log, "{}", json!({"type":"turn_context", "timestamp":time,
                "payload":{"turn_id":turn,"model":model,"service_tier":"standard","cwd":"/projects/defaults"}})).unwrap();
            writeln!(log, "{}", json!({"type":"token_usage_record", "timestamp":time,
                "payload":{"session_id":"defaults-session","thread_id":"defaults-session","turn_id":turn,"response_id":format!("response-{index}"),
                    "usage":{"input_tokens":input,"cached_input_tokens":0,"cache_write_input_tokens":0,
                        "output_tokens":input/10,"reasoning_output_tokens":0,"total_tokens":input+input/10}}})).unwrap();
        }
        Self {
            _dir: dir,
            root,
            config,
            today,
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_codex-usage"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }
    fn report(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        success(&output);
        assert!(!output.stdout.contains(&0x1b));
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "invalid report JSON: {e}\n{}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }
    fn snapshots(&self, args: &[&str]) -> Vec<Value> {
        let output = self.run(args);
        success(&output);
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn root(&self) -> &str {
        self.root.to_str().unwrap()
    }
    fn preference(&self) -> PathBuf {
        self.config.with_file_name("config.toml.last-report.json")
    }
    fn filtered_daily(&self) -> Value {
        self.report(&[
            "daily",
            "--last",
            "7",
            "--root",
            self.root(),
            "--format",
            "json",
            "--timezone",
            "UTC",
            "--no-cost",
            "--model",
            "alpha",
            "--sort",
            "model",
            "--exact",
            "--wide",
        ])
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
fn midnight(date: NaiveDate) -> String {
    format!("{date}T00:00:00Z")
}
fn assert_models(report: &Value, model: &str) {
    assert!(!report["rows"].as_array().unwrap().is_empty());
    assert!(report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["dimensions"]["model"] == model));
}

#[test]
fn bare_command_falls_back_to_last_seven_days_without_quota() {
    let f = Fixture::new();
    let report = f.report(&[
        "--root",
        f.root(),
        "--format",
        "json",
        "--timezone",
        "UTC",
        "--no-cost",
    ]);
    assert_eq!(report["kind"], "Daily");
    assert_eq!(report["period"]["basis"], "last_7_days_fallback");
    assert_eq!(
        report["period"]["start"],
        midnight(f.today.checked_sub_days(Days::new(6)).unwrap())
    );
    assert_eq!(
        report["period"]["end"],
        midnight(f.today.checked_add_days(Days::new(1)).unwrap())
    );
    assert!(!f.preference().exists());
}

#[test]
fn report_exports_and_filters_do_not_change_later_defaults() {
    let f = Fixture::new();
    let filtered = f.filtered_daily();
    assert_models(&filtered, "alpha");
    assert_eq!(filtered["totals"]["total"], 330);
    for command in [Some("daily"), None] {
        let mut args = vec!["--root", f.root(), "--timezone", "UTC", "--no-cost"];
        if let Some(command) = command {
            args.insert(0, command);
        }
        let output = f.run(&args);
        success(&output);
        let table = String::from_utf8(output.stdout).unwrap();
        assert!(
            !table.trim_start().starts_with('{'),
            "JSON must be explicit: {table}"
        );
        assert!(
            table.contains("Daily") && table.contains("alpha") && table.contains("beta"),
            "{table}"
        );
    }
    let fresh = f.report(&[
        "daily",
        "--root",
        f.root(),
        "--format",
        "json",
        "--timezone",
        "UTC",
    ]);
    assert_ne!(fresh["totals"]["pricing_status"], "disabled");
    assert_eq!(
        fresh["period"]["start"],
        midnight(f.today.checked_sub_days(Days::new(6)).unwrap())
    );
    assert!(fresh["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["dimensions"]["model"] == "beta"));
    assert!(!f.preference().exists());
}

#[test]
fn switching_commands_never_carries_report_options() {
    let f = Fixture::new();
    f.filtered_daily();
    for (command, kind) in [
        ("models", "Models"),
        ("monthly", "Monthly"),
        ("sessions", "Sessions"),
    ] {
        let report = f.report(&[
            command,
            "--root",
            f.root(),
            "--all",
            "--format",
            "json",
            "--no-cost",
        ]);
        assert_eq!(report["kind"], kind);
        assert_eq!(report["totals"]["total"], 1650, "{command}: {report}");
    }
    let bare = f.report(&[
        "--root",
        f.root(),
        "--format",
        "json",
        "--timezone",
        "UTC",
        "--no-cost",
    ]);
    assert_eq!(bare["kind"], "Daily");
    assert_eq!(
        bare["period"]["start"],
        midnight(f.today.checked_sub_days(Days::new(6)).unwrap())
    );
}

#[test]
fn monitor_is_explicit_and_its_interval_and_filters_start_fresh() {
    let f = Fixture::new();
    let first = f.snapshots(&[
        "monitor",
        "--root",
        f.root(),
        "--interval",
        "1s",
        "--frames",
        "1",
        "--format",
        "jsonl",
        "--timezone",
        "UTC",
        "--no-cost",
        "--model",
        "alpha",
    ]);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["monitor"]["window_seconds"], 1.0);
    assert_models(&first[0], "alpha");
    let fresh = f.snapshots(&[
        "monitor",
        "--root",
        f.root(),
        "--frames",
        "1",
        "--timezone",
        "UTC",
        "--no-cost",
    ]);
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0]["kind"], "Monitor");
    assert_eq!(fresh[0]["monitor"]["window_seconds"], 2.0);
    assert_eq!(fresh[0]["totals"]["total"], 440);
    let bare = f.report(&[
        "--root",
        f.root(),
        "--format",
        "json",
        "--timezone",
        "UTC",
        "--no-cost",
    ]);
    assert_eq!(bare["kind"], "Daily");
    assert!(bare.get("monitor").is_none_or(Value::is_null));
    assert!(!f.preference().exists());
}

#[test]
fn old_history_files_are_ignored_and_left_untouched() {
    let f = Fixture::new();
    fs::create_dir_all(f.config.parent().unwrap()).unwrap();
    for sentinel in [
        "{ corrupt legacy history",
        r#"{"schema_version":1,"report":{"command":{"Monitor":{"interval":{"secs":3600,"nanos":0},"frames":null}},"format":"Json","models":["missing"],"roots":["/missing/history-root"],"last":1}}"#,
    ] {
        fs::write(f.preference(), sentinel).unwrap();
        let report = f.report(&[
            "--root",
            f.root(),
            "--format",
            "json",
            "--timezone",
            "UTC",
            "--no-cost",
        ]);
        assert_eq!(report["kind"], "Daily");
        assert_eq!(
            report["period"]["start"],
            midnight(f.today.checked_sub_days(Days::new(6)).unwrap())
        );
        assert!(!report["rows"].as_array().unwrap().is_empty());
        assert_eq!(fs::read_to_string(f.preference()).unwrap(), sentinel);
    }
}

#[test]
fn source_and_timezone_configuration_remain_persistent_per_profile() {
    let a = Fixture::new();
    let b = Fixture::new();
    for (f, name, timezone) in [(&a, "first", "Asia/Shanghai"), (&b, "second", "UTC")] {
        success(&f.run(&[
            "sources",
            "set",
            name,
            "--kind",
            "linux",
            "--root",
            f.root(),
        ]));
        let config = fs::read_to_string(&f.config).unwrap();
        fs::write(&f.config, format!("timezone = '{timezone}'\n{config}")).unwrap();
        f.filtered_daily();
        for _ in 0..2 {
            let report = f.report(&["--format", "json", "--no-cost"]);
            assert_eq!(report["kind"], "Daily");
            assert_eq!(report["period"]["timezone"], timezone);
            assert_eq!(report["sources"][0]["name"], name);
            assert_eq!(report["sources"][0]["root"], f.root());
            assert!(report["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["dimensions"]["model"] == "beta"));
        }
        assert!(!f.preference().exists());
    }
}

#[test]
fn last_is_a_calendar_window_for_only_the_current_invocation() {
    let f = Fixture::new();
    let report = f.filtered_daily();
    assert_eq!(
        report["period"]["start"],
        midnight(f.today.checked_sub_days(Days::new(6)).unwrap())
    );
    assert_eq!(
        report["period"]["end"],
        midnight(f.today.checked_add_days(Days::new(1)).unwrap())
    );
    let today = f.report(&[
        "daily",
        "--root",
        f.root(),
        "--last",
        "1",
        "--format",
        "json",
        "--timezone",
        "UTC",
        "--no-cost",
    ]);
    assert_eq!(today["period"]["start"], midnight(f.today));
    assert_eq!(today["totals"]["total"], 440);
    assert!(!f.preference().exists());
}

#[test]
fn invalid_periods_and_removed_reset_option_are_rejected() {
    let f = Fixture::new();
    for args in [
        vec!["daily", "--last", "0"],
        vec!["daily", "--last", "7", "--all"],
        vec!["daily", "--last", "7", "--today"],
        vec!["daily", "--last", "7", "--since", "2026-01-01"],
        vec!["daily", "--last", "7", "--until", "2026-12-31"],
        vec!["monitor", "--last", "7"],
        vec!["monitor", "--all"],
        vec!["monitor", "--since", "2026-01-01"],
        vec!["monitor", "--until", "2026-12-31"],
        vec!["--reset"],
        vec!["daily", "--reset"],
    ] {
        let output = f.run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "accepted invalid args {args:?}"
        );
        assert!(!f.preference().exists());
    }
}
