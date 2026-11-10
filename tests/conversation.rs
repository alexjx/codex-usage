//! Conversation exports and navigation entry contracts use synthetic local logs only.
use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, UNIX_EPOCH},
};
use tempfile::{tempdir, TempDir};

const SESSION: &str = "session-alpha";
const OTHER: &str = "session-beta";

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().join("logs");
        fs::create_dir_all(root.join("sessions")).unwrap();
        let config = dir.path().join("settings/config.toml");
        Self {
            _dir: dir,
            root,
            config,
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-usage"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        success(&output);
        assert!(!output.stdout.contains(&0x1b));
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
    }
    fn root(&self) -> &str {
        self.root.to_str().unwrap()
    }
    fn write(&self, name: &str, records: &[Value]) -> PathBuf {
        write(&self.root, name, records)
    }
}
fn write(root: &Path, name: &str, records: &[Value]) -> PathBuf {
    fs::create_dir_all(root.join("sessions")).unwrap();
    let path = root.join("sessions").join(format!("{name}.jsonl"));
    let mut file = fs::File::create(&path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
    path
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
fn message(id: &str, role: &str, text: &str, time: &str) -> Value {
    json!({"type":"response_item","timestamp":time,"payload":{"type":"message","id":id,"role":role,"content":[{"type":if role=="user" {"input_text"} else {"output_text"},"text":text}]}})
}
fn records(session: &str, time: &str) -> Vec<Value> {
    vec![
        json!({"type":"session_meta","timestamp":time,"payload":{"id":session,"session_id":session,"model_provider":"openai","cwd":"/projects/conversation"}}),
        json!({"type":"turn_context","timestamp":time,"payload":{"turn_id":"turn-one","model":"gpt-6.1-sol","service_tier":"standard","cwd":"/projects/conversation"}}),
        message("user-one", "user", "Please check the costs.", time),
        json!({"type":"event_msg","timestamp":time,"payload":{"type":"user_message","turn_id":"turn-one","message":"Please check the costs."}}),
        message(
            "assistant-progress",
            "assistant",
            "I will inspect the token counts.",
            time,
        ),
        json!({"type":"response_item","timestamp":time,"payload":{"type":"function_call","id":"tool-one","call_id":"call-one","name":"exec_command","arguments":"{\"cmd\":\"echo synthetic\"}"}}),
        json!({"type":"response_item","timestamp":time,"payload":{"type":"function_call_output","id":"result-one","call_id":"call-one","output":"synthetic tool result"}}),
        message(
            "assistant-final",
            "assistant",
            "The costs are correct.",
            time,
        ),
        json!({"type":"event_msg","timestamp":time,"payload":{"type":"agent_message","turn_id":"turn-one","message":"The costs are correct."}}),
        json!({"type":"token_usage_record","timestamp":time,"payload":{"response_id":"response-one","session_id":session,"thread_id":session,"turn_id":"turn-one","usage":{"input_tokens":100,"cached_input_tokens":60,"cache_write_input_tokens":0,"output_tokens":20,"reasoning_output_tokens":10,"total_tokens":120}}}),
        json!({"type":"turn_context","timestamp":time,"payload":{"turn_id":"turn-two","model":"gpt-6.1-sol","service_tier":"standard","cwd":"/projects/conversation"}}),
        message("user-two", "user", "Please check the costs.", time),
        message(
            "assistant-two",
            "assistant",
            "This is a second, distinct turn.",
            time,
        ),
    ]
}
fn blocks(conversation: &Value) -> Vec<&Value> {
    conversation["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["blocks"].as_array().unwrap())
        .collect()
}

#[test]
fn conversation_json_keeps_messages_tools_and_usage_independent() {
    let f = Fixture::new();
    f.write("session-alpha", &records(SESSION, &Utc::now().to_rfc3339()));
    let usage = f.json(&[
        "session",
        SESSION,
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--format",
        "json",
    ]);
    let explored = f.json(&[
        "session",
        SESSION,
        "--conversation",
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--format",
        "json",
    ]);
    assert_eq!(explored["schema_version"], 2);
    assert_eq!(explored["conversation"]["session"]["native_id"], SESSION);
    assert_eq!(explored["usage"]["totals"], usage["totals"]);
    assert_eq!(explored["usage"]["totals"]["total"], 120);
    assert_eq!(
        explored["usage"]["totals"]["cost_usd"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap(),
        Decimal::new(286, 6)
    );
    let blocks = blocks(&explored["conversation"]);
    assert_eq!(
        blocks.iter().filter(|b| b["kind"] == "user").count(),
        2,
        "mirrors must not duplicate messages, repeated user prompts across turns must remain"
    );
    assert_eq!(
        blocks.iter().filter(|b| b["kind"] == "assistant").count(),
        3
    );
    let tools: Vec<_> = blocks.iter().filter(|b| b["kind"] == "tool").collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["tool_name"], "exec_command");
    assert!(tools[0]["text"]
        .as_str()
        .unwrap()
        .contains("echo synthetic"));
    assert_eq!(tools[0]["output"], "synthetic tool result");
    assert_eq!(
        explored["conversation"]["turns"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn full_conversation_pipe_output_has_no_ansi_or_preview_truncation() {
    let f = Fixture::new();
    let time = Utc::now().to_rfc3339();
    let mut r = records(SESSION, &time);
    let long = (0..300)
        .map(|i| format!("retained line {i:03}\n"))
        .collect::<String>();
    r.push(message("long-assistant", "assistant", &long, &time));
    r.push(message(
        "control-assistant",
        "assistant",
        "safe\u{1b}[31m text\u{1b}]52;c;ZXhhbXBsZQ==\u{7}",
        &time,
    ));
    f.write("selected-session", &r);
    let output = f.run(&[
        "session",
        SESSION,
        "--conversation",
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--color",
        "always",
    ]);
    success(&output);
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stdout.contains(&0x07));
    let text = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "Please check the costs.",
        "The costs are correct.",
        "exec_command",
        "synthetic tool result",
        "retained line 000",
        "retained line 299",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
}

#[test]
fn conversation_lookup_ignores_report_mtime_cutoff_but_obeys_session_and_source_scope() {
    let f = Fixture::new();
    let old = f.write("old-log", &records(SESSION, "2020-09-13T12:00:00Z"));
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000)),
        )
        .unwrap();
    let mut other = records(OTHER, &Utc::now().to_rfc3339());
    other[2] = message(
        "other-user",
        "user",
        "UNRELATED SESSION TEXT",
        &Utc::now().to_rfc3339(),
    );
    f.write("other-log", &other);
    let result = f.json(&[
        "session",
        SESSION,
        "--conversation",
        "--today",
        "--timezone",
        "UTC",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--format",
        "json",
    ]);
    assert_eq!(
        result["usage"]["totals"]["total"], 0,
        "usage must retain report's today scope"
    );
    assert!(result["conversation"]["turns"].as_array().unwrap().len() >= 2);
    assert!(!serde_json::to_string(&result["conversation"])
        .unwrap()
        .contains("UNRELATED SESSION TEXT"));
    assert_eq!(result["conversation"]["session"]["native_id"], SESSION);
}

#[test]
fn copied_logs_do_not_duplicate_conversation_or_accounting() {
    let f = Fixture::new();
    let windows = f._dir.path().join("windows");
    let records = records(SESSION, &Utc::now().to_rfc3339());
    f.write("original-session", &records);
    write(&windows, "copied-session", &records);
    let report = f.json(&[
        "session",
        SESSION,
        "--conversation",
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--agent",
        "codex",
        "--root",
        windows.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(report["usage"]["totals"]["total"], 120);
    assert_eq!(
        blocks(&report["conversation"])
            .iter()
            .filter(|b| b["kind"] == "user")
            .count(),
        2
    );
    assert_eq!(
        blocks(&report["conversation"])
            .iter()
            .filter(|b| b["kind"] == "tool")
            .count(),
        1
    );
    assert_eq!(
        report["conversation"]["sources"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn conversation_and_monitor_options_do_not_carry_over_to_later_invocations() {
    let f = Fixture::new();
    f.write("session-alpha", &records(SESSION, &Utc::now().to_rfc3339()));
    fs::create_dir_all(f.config.parent().unwrap()).unwrap();
    let legacy_history = f.config.with_file_name("config.toml.last-report.json");
    let legacy_contents = b"legacy history must be ignored, even when invalid JSON";
    fs::write(&legacy_history, legacy_contents).unwrap();
    let first = f.json(&[
        "session",
        SESSION,
        "--conversation",
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--format",
        "json",
    ]);
    let usage = f.json(&[
        "session",
        SESSION,
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--format",
        "json",
    ]);
    assert_eq!(usage["kind"], "Session");
    assert!(usage.get("conversation").is_none());
    assert_eq!(usage["totals"], first["usage"]["totals"]);

    let daily = f.run(&["--agent", "codex", "--root", f.root(), "--color", "never"]);
    success(&daily);
    let text = String::from_utf8(daily.stdout).unwrap();
    assert!(text.contains("Agent Usage / Daily"), "{text}");
    assert!(!text.contains("Please check the costs."), "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_err());

    let monitor = f.run(&[
        "monitor",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--frames",
        "1",
        "--format",
        "jsonl",
    ]);
    success(&monitor);
    let daily = f.json(&["--agent", "codex", "--root", f.root(), "--format", "json"]);
    assert_eq!(daily["kind"], "Daily");
    assert!(daily.get("conversation").is_none());
    assert_eq!(daily["totals"], usage["totals"]);
    assert_eq!(fs::read(legacy_history).unwrap(), legacy_contents);
}

#[test]
fn conversation_reports_missing_sessions_and_rejects_inapplicable_formats() {
    let f = Fixture::new();
    f.write("session-alpha", &records(SESSION, &Utc::now().to_rfc3339()));
    for mut args in [
        vec!["session", SESSION, "--conversation", "--format", "csv"],
        vec!["session", SESSION, "--conversation", "--format", "jsonl"],
        vec!["models", "--conversation"],
        vec!["monitor", "--conversation", "--frames", "1"],
    ] {
        args.extend(["--agent", "codex", "--root", f.root()]);
        let output = f.run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "accepted {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            error.contains("conversation") || error.contains("jsonl"),
            "unexpected validation error: {error}"
        );
    }
    let missing = f.run(&[
        "session",
        "absent-session",
        "--conversation",
        "--agent",
        "codex",
        "--root",
        f.root(),
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr)
        .to_lowercase()
        .contains("session"));
}

#[test]
fn incomplete_trailing_messages_remain_pending_until_the_record_is_complete() {
    let f = Fixture::new();
    let time = Utc::now().to_rfc3339();
    let path = f.write("partial-session", &records(SESSION, &time));
    let next = message(
        "arriving-message",
        "assistant",
        "A complete arriving message.",
        &time,
    )
    .to_string();
    let split = next.len() / 2;
    let mut writer = fs::OpenOptions::new().append(true).open(path).unwrap();
    writer.write_all(&next.as_bytes()[..split]).unwrap();
    writer.flush().unwrap();
    let pending = f.json(&[
        "session",
        SESSION,
        "--conversation",
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--format",
        "json",
    ]);
    assert_eq!(pending["conversation"]["pending"], 1);
    assert!(!serde_json::to_string(&pending["conversation"])
        .unwrap()
        .contains("A complete arriving message."));
    writer.write_all(&next.as_bytes()[split..]).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
    let complete = f.json(&[
        "session",
        SESSION,
        "--conversation",
        "--all",
        "--agent",
        "codex",
        "--root",
        f.root(),
        "--format",
        "json",
    ]);
    assert_eq!(complete["conversation"]["pending"], 0);
    assert_eq!(
        blocks(&complete["conversation"])
            .iter()
            .filter(|b| b["text"] == "A complete arriving message.")
            .count(),
        1
    );
    assert_eq!(complete["usage"]["totals"], pending["usage"]["totals"]);
}

#[test]
fn conversation_can_read_retained_compressed_archives() {
    let f = Fixture::new();
    let archive = f.root.join("archived_sessions");
    fs::create_dir_all(&archive).unwrap();
    let raw = records(SESSION, "2020-09-13T12:00:00Z")
        .iter()
        .map(|r| format!("{r}\n"))
        .collect::<String>();
    fs::write(
        archive.join("old-session.jsonl.zst"),
        zstd::stream::encode_all(raw.as_bytes(), 0).unwrap(),
    )
    .unwrap();
    let output = f.run(&[
        "session",
        SESSION,
        "--conversation",
        "--today",
        "--agent",
        "codex",
        "--root",
        f.root(),
    ]);
    success(&output);
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("The costs are correct."));
}
