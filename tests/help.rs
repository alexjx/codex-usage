use agent_usage::cli::Cli;
use clap::CommandFactory;
use std::process::Command;

#[test]
fn every_command_and_argument_has_a_help_description() {
    fn check(command: &clap::Command) {
        assert!(
            command
                .get_about()
                .is_some_and(|text| !text.to_string().trim().is_empty()),
            "Missing command description: {}",
            command.get_name()
        );
        for arg in command.get_arguments() {
            assert!(
                arg.get_help()
                    .is_some_and(|text| !text.to_string().trim().is_empty()),
                "Missing argument description: {} / {}",
                command.get_name(),
                arg.get_id()
            );
        }
        for subcommand in command.get_subcommands().filter(|c| c.get_name() != "help") {
            check(subcommand);
        }
    }
    let mut command = Cli::command();
    command.build();
    check(&command);
}

fn help(path: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-usage"))
        .args(path)
        .arg("--help")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{path:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn every_help_path_has_examples_and_action_specific_options() {
    for path in [
        vec![],
        vec!["models"],
        vec!["daily"],
        vec!["monthly"],
        vec!["sessions"],
        vec!["session"],
        vec!["monitor"],
        vec!["sources"],
        vec!["pricing"],
        vec!["doctor"],
        vec!["sources", "set"],
        vec!["sources", "enable"],
        vec!["sources", "disable"],
    ] {
        let text = help(&path);
        assert!(
            text.contains("Usage:") && text.contains("Example"),
            "{path:?}: {text}"
        );
    }
    let root = help(&[]);
    for description in [
        "With no subcommand, run daily",
        "last 7 days, including today",
        "fresh command options",
        "Ctrl+C",
    ] {
        assert!(root.contains(description), "{root}");
    }
    assert!(!root.contains("--reset"), "{root}");
    let monitor = help(&["monitor"]);
    for description in [
        "1000ms",
        "initial baseline",
        "jsonl when redirected",
        "60s per-session",
    ] {
        assert!(monitor.contains(description), "{monitor}");
    }
    for flag in ["--today", "--last", "--all", "--since", "--until"] {
        assert!(
            !monitor
                .lines()
                .any(|line| line.trim_start().starts_with(flag)),
            "{monitor}"
        );
    }
    for path in [
        vec!["sources"],
        vec!["sources", "set"],
        vec!["sources", "enable"],
        vec!["sources", "disable"],
        vec!["pricing"],
    ] {
        let text = help(&path);
        for flag in ["--model", "--today", "--group-by", "--pager", "--no-cost"] {
            assert!(
                !text.lines().any(|line| line.trim_start().starts_with(flag)),
                "{path:?}: {text}"
            );
        }
        assert!(text.contains("--config"), "{text}");
    }
    let set = help(&["sources", "set"]);
    assert!(
        set.contains("--root <PATH>") && set.contains("--kind") && set.contains("<NAME>"),
        "{set}"
    );
    assert!(help(&["pricing"]).contains("--pricing-file"));
}

#[test]
fn built_in_help_command_resolves_nested_action_help() {
    for args in [
        vec!["help", "monitor"],
        vec!["help", "sources", "set"],
        vec!["sources", "help", "enable"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-usage"))
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("Usage:") && text.contains("Example"),
            "{text}"
        );
    }
}

#[test]
fn monitor_interval_and_frame_defaults_do_not_depend_on_prior_parses() {
    let matches = Cli::command()
        .try_get_matches_from(["agent-usage", "monitor"])
        .unwrap();
    let monitor = matches.subcommand_matches("monitor").unwrap();
    assert_eq!(
        monitor.get_one::<std::time::Duration>("interval"),
        Some(&std::time::Duration::from_secs(2))
    );
    assert_eq!(monitor.get_one::<u64>("frames"), None);
    let overridden = Cli::command()
        .try_get_matches_from([
            "agent-usage",
            "monitor",
            "--interval",
            "5s",
            "--frames",
            "3",
            "--model",
            "test",
        ])
        .unwrap();
    let monitor = overridden.subcommand_matches("monitor").unwrap();
    assert_eq!(
        monitor.get_one::<std::time::Duration>("interval"),
        Some(&std::time::Duration::from_secs(5))
    );
    assert_eq!(monitor.get_one::<u64>("frames"), Some(&3));
    let fresh = Cli::command()
        .try_get_matches_from(["agent-usage", "monitor"])
        .unwrap();
    let monitor = fresh.subcommand_matches("monitor").unwrap();
    assert_eq!(
        monitor.get_one::<std::time::Duration>("interval"),
        Some(&std::time::Duration::from_secs(2))
    );
    assert_eq!(monitor.get_one::<u64>("frames"), None);
}
