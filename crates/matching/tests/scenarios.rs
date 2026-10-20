//! Runs every `scenarios/*.json` file through a fresh matcher and compares the
//! events of each step with the expected ones, exactly.

use std::fs;
use std::path::{Path, PathBuf};

use matching::Matcher;
use protocol::{Command, Event};
use serde_json::Value;

fn scenario_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entries are readable").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

fn json_line(event: &Event) -> String {
    serde_json::to_string(event).expect("events serialize")
}

/// One line per event position: unchanged ones with a space, differing ones as
/// an expected (`-`) and an actual (`+`) line.
fn diff(expected: &[Event], actual: &[Event]) -> String {
    let mut lines = Vec::new();
    for i in 0..expected.len().max(actual.len()) {
        match (expected.get(i), actual.get(i)) {
            (Some(e), Some(a)) if e == a => lines.push(format!("  {}", json_line(e))),
            (e, a) => {
                lines.extend(e.map(|e| format!("- {}", json_line(e))));
                lines.extend(a.map(|a| format!("+ {}", json_line(a))));
            }
        }
    }
    lines.join("\n")
}

/// Runs the scenario in `text`; the error names the failing step.
fn run(text: &str) -> Result<(), String> {
    let scenario: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if !scenario["description"]
        .as_str()
        .is_some_and(|d| !d.is_empty())
    {
        return Err("`description` must be a non-empty string".into());
    }
    let steps = scenario["steps"]
        .as_array()
        .ok_or("`steps` must be an array")?;
    let mut matcher = Matcher::new();
    for (index, step) in steps.iter().enumerate() {
        let at = |message: String| format!("step {index}: {message}");
        let command: Command = serde_json::from_value(step["command"].clone())
            .map_err(|e| at(format!("invalid command: {e}")))?;
        let expected: Vec<Event> = serde_json::from_value(step["events"].clone())
            .map_err(|e| at(format!("invalid events: {e}")))?;
        let actual = matcher.apply(command);
        if actual != expected {
            return Err(at(format!(
                "events differ for {command:?} (- expected, + actual)\n{}",
                diff(&expected, &actual)
            )));
        }
    }
    Ok(())
}

#[test]
fn every_scenario_file_produces_its_expected_events() {
    let files = scenario_files();
    assert!(!files.is_empty(), "no scenarios found");
    let failures: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let text = fs::read_to_string(path).expect("scenario files are readable");
            let name = path
                .file_name()
                .expect("files have names")
                .to_string_lossy();
            run(&text).err().map(|e| format!("{name}, {e}"))
        })
        .collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn a_mismatch_names_the_step_and_shows_the_differing_events() {
    let scenario = r#"{
        "description": "second step expects the wrong level total",
        "steps": [
            {
                "command": { "type": "place", "order_id": 1, "side": "buy",
                    "order_type": { "type": "limit", "price": 100, "time_in_force": "gtc" }, "qty": 5 },
                "events": [
                    { "type": "accepted", "order_id": 1, "side": "buy",
                        "order_type": { "type": "limit", "price": 100, "time_in_force": "gtc" }, "qty": 5 },
                    { "type": "l2_update", "side": "buy", "price": 100, "qty": 5 }
                ]
            },
            {
                "command": { "type": "cancel", "order_id": 1 },
                "events": [
                    { "type": "canceled", "order_id": 1, "leaves_qty": 5 },
                    { "type": "l2_update", "side": "buy", "price": 100, "qty": 1 }
                ]
            }
        ]
    }"#;
    let error = run(scenario).unwrap_err();
    assert!(error.starts_with("step 1: events differ"), "{error}");
    assert!(error.contains("  {\"type\":\"canceled\""), "{error}");
    assert!(error.contains("- {\"type\":\"l2_update\",\"side\":\"buy\",\"price\":100,\"qty\":1}"));
    assert!(error.contains("+ {\"type\":\"l2_update\",\"side\":\"buy\",\"price\":100,\"qty\":0}"));
}

#[test]
fn an_unknown_command_field_is_rejected() {
    let scenario = r#"{
        "description": "commands are parsed strictly",
        "steps": [
            { "command": { "type": "cancel", "order_id": 1, "qty": 1 }, "events": [] }
        ]
    }"#;
    let error = run(scenario).unwrap_err();
    assert!(error.starts_with("step 0: invalid command"), "{error}");
    assert!(error.contains("unknown field `qty`"), "{error}");
}
