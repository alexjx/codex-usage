//! Real SIGINT during a large, single-record file read (Linux process signals).
#![cfg(target_os = "linux")]

use std::{
    fs::{self, File},
    io::Write,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn ctrl_c_cancels_report_and_monitor_loading_without_a_partial_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("logs");
    fs::create_dir_all(root.join("sessions")).unwrap();
    let path = root.join("sessions/large.jsonl");
    let config = dir.path().join("config.toml");
    let history = dir.path().join("config.toml.last-report.json");
    let baseline = Command::new(env!("CARGO_BIN_EXE_codex-usage"))
        .args(["models", "--all", "--no-cost", "--format", "json"])
        .arg("--config")
        .arg(&config)
        .arg("--root")
        .arg(&root)
        .output()
        .unwrap();
    assert!(baseline.status.success());
    let previous = fs::read(&history).unwrap();
    let mut file = File::create(&path).unwrap();
    file.write_all(b"{\"type\":\"response_item\",\"payload\":\"")
        .unwrap();
    // Sparse fixture forces chunk cancellation inside read_until, without disk use.
    file.set_len(2 * 1024 * 1024 * 1024).unwrap();
    drop(file);
    for (command, format) in [("daily", "json"), ("monitor", "jsonl")] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codex-usage"))
            .args([command, "--no-cost", "--format", format])
            .arg("--config")
            .arg(&config)
            .arg("--root")
            .arg(&root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reading = false;
        while Instant::now() < deadline {
            if let Ok(entries) = fs::read_dir(format!("/proc/{}/fd", child.id())) {
                reading = entries
                    .flatten()
                    .any(|fd| fs::read_link(fd.path()).is_ok_and(|p| p == path));
            }
            if reading {
                break;
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        if !reading {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{command} never started reading the fixture");
        }
        assert!(Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{command} ignored Ctrl+C while loading");
            }
            thread::sleep(Duration::from_millis(2));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(130), "{command}: {output:?}");
        assert!(
            output.stdout.is_empty(),
            "Cancelled load emitted a partial report"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            "Interrupted"
        );
        assert_eq!(fs::read(&history).unwrap(), previous);
    }
}
