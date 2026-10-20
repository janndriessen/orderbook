//! Runs every `scenarios/*.json` file through a fresh matcher and compares the
//! events of each step with the expected ones, exactly.

use std::fs;
use std::path::{Path, PathBuf};

use matching::Matcher;
use protocol::Command;
use serde_json::{Map, Value};

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

/// One line per event position: unchanged ones with a space, differing ones as
/// an expected (`-`) and an actual (`+`) line.
fn diff(expected: &[Value], actual: &[Value]) -> String {
    let mut lines = Vec::new();
    for i in 0..expected.len().max(actual.len()) {
        match (expected.get(i), actual.get(i)) {
            (Some(e), Some(a)) if e == a => lines.push(format!("  {e}")),
            (e, a) => {
                lines.extend(e.map(|e| format!("- {e}")));
                lines.extend(a.map(|a| format!("+ {a}")));
            }
        }
    }
    lines.join("\n")
}

/// Fails if `object` has a key outside `allowed`.
fn only_keys(object: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), String> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("{what} has an unknown key `{key}`")),
        None => Ok(()),
    }
}

/// Runs the scenario in `text`; the error names the failing step, counted from 1.
///
/// Each command is printed before it runs, so a panic inside the matcher (an
/// input the gateway would have refused) still shows the step in the captured
/// output of the failing test.
fn run(text: &str) -> Result<(), String> {
    let scenario: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let scenario = scenario.as_object().ok_or("a scenario must be an object")?;
    only_keys(scenario, &["description", "steps"], "the scenario")?;
    if !scenario
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|d| !d.is_empty())
    {
        return Err("`description` must be a non-empty string".into());
    }
    let steps = scenario
        .get("steps")
        .and_then(Value::as_array)
        .ok_or("`steps` must be an array")?;
    if steps.is_empty() {
        return Err("`steps` must have at least one step".into());
    }
    let mut matcher = Matcher::new();
    for (index, step) in steps.iter().enumerate() {
        let number = index + 1;
        let at = |message: String| format!("step {number}: {message}");
        let step = step
            .as_object()
            .ok_or_else(|| at("a step must be an object".into()))?;
        only_keys(step, &["command", "events"], &at("the step".into()))?;
        let command: Command =
            serde_json::from_value(step.get("command").cloned().unwrap_or(Value::Null))
                .map_err(|e| at(format!("invalid command: {e}")))?;
        let expected = step
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| at("`events` must be an array".into()))?;
        println!("step {number}: {command:?}");
        let actual: Vec<Value> = matcher
            .apply(command)
            .iter()
            .map(|event| serde_json::to_value(event).expect("events serialize"))
            .collect();
        if actual != *expected {
            return Err(at(format!(
                "events differ for {command:?} (- expected, + actual)\n{}",
                diff(expected, &actual)
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
            println!("{name}");
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
    assert!(error.starts_with("step 2: events differ"), "{error}");
    assert!(
        error.contains("  {\"leaves_qty\":5,\"order_id\":1,\"type\":\"canceled\"}"),
        "{error}"
    );
    assert!(error.contains("- {\"price\":100,\"qty\":1,\"side\":\"buy\",\"type\":\"l2_update\"}"));
    assert!(error.contains("+ {\"price\":100,\"qty\":0,\"side\":\"buy\",\"type\":\"l2_update\"}"));
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
    assert!(error.starts_with("step 1: invalid command"), "{error}");
    assert!(error.contains("unknown field `qty`"), "{error}");
}

const PLACE: &str = r#"{ "type": "place", "order_id": 1, "side": "buy",
    "order_type": { "type": "limit", "price": 100, "time_in_force": "gtc" }, "qty": 5 }"#;

fn accepted(extra: &str) -> String {
    format!(
        r#"{{ "type": "accepted", "order_id": 1, "side": "buy", "qty": 5{extra},
            "order_type": {{ "type": "limit", "price": 100, "time_in_force": "gtc" }} }}"#
    )
}

fn one_step(events: &str) -> String {
    format!(
        r#"{{ "description": "d", "steps": [ {{ "command": {PLACE}, "events": [{events}] }} ] }}"#
    )
}

#[test]
fn a_stray_field_in_an_expected_event_fails() {
    let level = r#"{ "type": "l2_update", "side": "buy", "price": 100, "qty": 5 }"#;
    let ok = one_step(&format!("{}, {level}", accepted("")));
    assert_eq!(run(&ok), Ok(()));
    let stray = one_step(&format!("{}, {level}", accepted(r#", "extra": 1"#)));
    let error = run(&stray).unwrap_err();
    assert!(error.starts_with("step 1: events differ"), "{error}");
}

#[test]
fn a_scenario_without_steps_is_rejected() {
    let error = run(r#"{ "description": "d", "steps": [] }"#).unwrap_err();
    assert!(error.contains("at least one step"), "{error}");
}

#[test]
fn unknown_scenario_and_step_keys_are_rejected() {
    let error = run(r#"{ "description": "d", "steps": [], "note": "x" }"#).unwrap_err();
    assert_eq!(error, "the scenario has an unknown key `note`");
    let step = r#"{ "description": "d", "steps": [
        { "command": { "type": "cancel", "order_id": 1 }, "events": [], "comment": "x" } ] }"#;
    let error = run(step).unwrap_err();
    assert_eq!(error, "step 1: the step has an unknown key `comment`");
}
