# Scenarios

Hand-written, deterministic test cases for the matching engine. Each `*.json` file is `{ "description": "...", "steps": [ { "command": ..., "events": [...] } ] }`, where `command` and every event use the wire format of `crates/protocol`.

`cargo test -p matching` runs each file through a fresh matcher, feeds it the commands in order and compares the events of every step exactly: the events the matcher produced, serialized to JSON, must equal the `events` array of the file, so a missing, extra, misspelled or stray field fails the test. A failure names the file and the step and shows the differing events.

The format is strict:

- `description` must be a non-empty string and `steps` must have at least one step.
- The top level accepts only `description` and `steps`; a step accepts only `command` and `events`. Anything else, such as a `note` key, is an error.
- Commands are parsed strictly too, so a misspelled field fails the test.
- Steps are numbered from 1 in messages, the way you count them in the file.

Scenarios feed the matcher directly, without the gateway in front of it. Inputs the gateway refuses (a duplicate order id, a zero quantity) are therefore not valid scenario input: the matcher panics on them, and the failure shows the file and the step that caused it.
