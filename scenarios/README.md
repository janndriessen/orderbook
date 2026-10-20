# Scenarios

Hand-written, deterministic test cases for the matching engine. Each `*.json` file is `{ "description": "...", "steps": [ { "command": ..., "events": [...] } ] }`, where `command` and every event use the wire format of `crates/protocol`; commands are parsed strictly, so a misspelled field fails the test.

`cargo test -p matching` runs each file through a fresh matcher, feeds it the commands in order and compares the events of every step exactly. A failure names the file and step and shows the differing events.
