//! Last-report preferences and calendar ranges, verified through the real binary.
//! Every command uses isolated temporary configuration and synthetic logs.
use chrono::{Datelike, Days, NaiveDate, Utc};
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
        let mut log = fs::File::create(root.join("sessions/rollout-history-test.jsonl")).unwrap();
        writeln!(log, "{}", json!({"type":"session_meta", "timestamp":format!("{start}T00:00:00Z"),
            "payload":{"id":"history-session","session_id":"history-session","model_provider":"openai","cwd":"/projects/history"}})).unwrap();
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
                "payload":{"turn_id":turn,"model":model,"service_tier":"standard","cwd":"/projects/history"}})).unwrap();
            writeln!(log, "{}", json!({"type":"token_usage_record", "timestamp":time,
                "payload":{"session_id":"history-session","thread_id":"history-session","turn_id":turn,"response_id":format!("response-{index}"),
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
    fn root(&self) -> &str {
        self.root.to_str().unwrap()
    }
    fn preference(&self) -> PathBuf {
        self.config.with_file_name("config.toml.last-report.json")
    }
    fn save_daily(&self) -> Value {
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
fn first_bare_invocation_starts_with_this_months_model_report() {
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
    assert_eq!(report["kind"], "Models");
    assert_eq!(
        report["period"]["start"],
        midnight(f.today.with_day(1).unwrap())
    );
    assert_eq!(f.report(&[])["rows"], report["rows"]);
}

#[test]
fn bare_invocation_repeats_full_report_and_explicit_bare_flags_replace_fields() {
    let f = Fixture::new();
    let first = f.save_daily();
    assert_eq!(first["kind"], "Daily");
    assert_eq!(
        first["period"]["start"],
        midnight(f.today.checked_sub_days(Days::new(6)).unwrap())
    );
    assert_eq!(
        first["period"]["end"],
        midnight(f.today.checked_add_days(Days::new(1)).unwrap())
    );
    assert_eq!(first["totals"]["total"], 330, "{first}");
    assert_models(&first, "alpha");
    let repeated = f.report(&[]);
    for field in ["kind", "period", "rows", "totals", "sources", "pricing"] {
        if field == "sources" {
            // Source scan timestamps legitimately differ between separate runs.
            assert_eq!(repeated[field][0]["root"], first[field][0]["root"]);
        } else {
            assert_eq!(
                repeated[field], first[field],
                "changed remembered field {field}"
            );
        }
    }
    assert_eq!(repeated["totals"]["pricing_status"], "disabled");
    let overridden = f.report(&["--last", "1", "--model", "beta"]);
    assert_eq!(overridden["kind"], "Daily");
    assert_eq!(overridden["period"]["start"], midnight(f.today));
    assert_eq!(overridden["totals"]["total"], 330);
    assert_models(&overridden, "beta");
    assert_eq!(f.report(&[])["rows"], overridden["rows"]);
    let stored: Value = serde_json::from_slice(&fs::read(f.preference()).unwrap()).unwrap();
    assert_eq!(stored["report"]["last"], 1);
    assert_eq!(stored["report"]["models"], json!(["beta"]));
    assert_eq!(stored["report"]["exact"], true);
    assert_eq!(stored["report"]["wide"], true);
}

#[test]
fn explicit_commands_start_fresh_with_current_month_and_become_new_default() {
    let f = Fixture::new();
    f.save_daily();
    let models = f.report(&[
        "models",
        "--root",
        f.root(),
        "--format",
        "json",
        "--timezone",
        "UTC",
        "--no-cost",
    ]);
    assert_eq!(models["kind"], "Models");
    assert_eq!(
        models["period"]["start"],
        midnight(f.today.with_day(1).unwrap())
    );
    assert!(models["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["dimensions"]["model"] == "beta"));
    assert_eq!(f.report(&[])["kind"], "Models");
    let daily = f.report(&[
        "daily",
        "--root",
        f.root(),
        "--format",
        "json",
        "--timezone",
        "UTC",
        "--no-cost",
    ]);
    assert_eq!(
        daily["period"]["start"],
        midnight(f.today.with_day(1).unwrap())
    );
    assert_eq!(f.report(&[])["kind"], "Daily");
}

#[test]
fn failed_reports_and_nonreport_commands_do_not_replace_last_report() {
    let f = Fixture::new();
    f.save_daily();
    let saved = fs::read(f.preference()).unwrap();
    let failed = f.run(&[
        "models",
        "--source",
        "missing-source",
        "--format",
        "json",
        "--no-cost",
    ]);
    assert!(!failed.status.success());
    assert_eq!(fs::read(f.preference()).unwrap(), saved);
    let missing = f._dir.path().join("missing-logs");
    let failed = f.run(&[
        "models",
        "--root",
        missing.to_str().unwrap(),
        "--format",
        "json",
        "--no-cost",
    ]);
    assert!(!failed.status.success());
    assert_eq!(fs::read(f.preference()).unwrap(), saved);
    for args in [
        vec![
            "sources",
            "set",
            "local",
            "--kind",
            "linux",
            "--root",
            f.root(),
        ],
        vec!["sources", "disable", "local"],
        vec!["sources", "enable", "local"],
        vec![
            "monitor",
            "--root",
            f.root(),
            "--frames",
            "1",
            "--format",
            "jsonl",
            "--no-cost",
        ],
        vec![
            "doctor",
            "--root",
            f.root(),
            "--format",
            "json",
            "--no-cost",
        ],
        vec!["pricing", "--format", "json"],
    ] {
        success(&f.run(&args));
        assert_eq!(
            fs::read(f.preference()).unwrap(),
            saved,
            "overwritten by {args:?}"
        );
    }
    assert_eq!(f.report(&[])["kind"], "Daily");
}

#[test]
fn explicit_bare_source_selection_replaces_remembered_ad_hoc_roots() {
    let f = Fixture::new();
    f.save_daily();
    success(&f.run(&[
        "sources",
        "set",
        "named",
        "--kind",
        "linux",
        "--root",
        f.root(),
    ]));
    let report = f.report(&["--source", "named"]);
    assert_eq!(report["kind"], "Daily");
    assert_eq!(report["sources"][0]["name"], "named");
    assert_eq!(report["totals"]["total"], 330);
    let stored: Value = serde_json::from_slice(&fs::read(f.preference()).unwrap()).unwrap();
    assert_eq!(stored["report"]["roots"], json!([]));
    assert_eq!(stored["report"]["sources"], json!(["named"]));
}

#[test]
fn configuration_profiles_keep_independent_report_preferences() {
    let a = Fixture::new();
    let b = Fixture::new();
    a.save_daily();
    b.report(&[
        "monthly",
        "--all",
        "--model",
        "beta",
        "--root",
        b.root(),
        "--format",
        "json",
        "--no-cost",
        "--timezone",
        "UTC",
    ]);
    let report_a = a.report(&[]);
    let report_b = b.report(&[]);
    assert_eq!(report_a["kind"], "Daily");
    assert_models(&report_a, "alpha");
    assert_eq!(report_b["kind"], "Monthly");
    assert_eq!(report_b["period"]["start"], Value::Null);
    assert_models(&report_b, "beta");
    assert_eq!(report_a["sources"][0]["root"], a.root());
    assert_eq!(report_b["sources"][0]["root"], b.root());
}

#[test]
fn zero_last_and_conflicting_periods_are_rejected_without_saving() {
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
