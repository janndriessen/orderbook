//! The `record` binary: writes the live BTC book and trades from Hyperliquid's
//! public WebSocket to a JSON Lines file, one received message per line.
//!
//! Two book feeds are subscribed: the standard one, 20 levels per side about
//! every 5 seconds, and the fast one, 5 levels per side about every half
//! second. Their messages have the same shape; only the depth tells them apart.
//!
//! Every message is parsed and written back re-serialized (keys sorted), with
//! any `users` field, which carries wallet addresses, removed at every depth.
//! A message that is not valid JSON is dropped. A closed or lost connection,
//! or 30 seconds without data, ends the run with an error: there is no
//! reconnect, because a gap in a recording would be silent.
//!
//! Settings come from the environment: `RECORD_PATH` (default
//! `data/hyperliquid-btc-<unix seconds>.jsonl`; an existing file is never
//! overwritten), `RECORD_SECONDS` (optional, above zero; stop after that long,
//! otherwise run until Ctrl-C) and `RUST_LOG` (log filter, default `info`).

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, interval_at, sleep};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const URL: &str = "wss://api.hyperliquid.xyz/ws";

/// How long the feed may stay silent before the connection counts as dead. The
/// standard book updates about every 5 seconds.
const IDLE_LIMIT: Duration = Duration::from_secs(30);

/// How often the documented application-level ping is sent, so that an idle
/// rule on the server side cannot close the connection.
const PING_EVERY: Duration = Duration::from_secs(30);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Removes every `users` field at any depth, whatever the channel, so that a
/// change in the shape of a message cannot let wallet addresses through.
fn strip_users(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("users");
            map.values_mut().for_each(strip_users);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_users),
        _ => {}
    }
}

/// Reads messages into `out` until the time limit or Ctrl-C; any other end is
/// an error.
async fn record(
    socket: &mut Socket,
    out: &mut BufWriter<File>,
    limit: Option<Duration>,
) -> anyhow::Result<()> {
    let stop = sleep(limit.unwrap_or(Duration::MAX));
    let idle = sleep(IDLE_LIMIT);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(stop, idle, ctrl_c);
    let mut ping = interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let mut lines = 0u64;
    loop {
        tokio::select! {
            () = &mut stop => return Ok(()),
            signal = &mut ctrl_c => {
                signal.context("cannot listen for ctrl-c")?;
                return Ok(());
            }
            () = &mut idle => bail!("no data for {IDLE_LIMIT:?} after {lines} lines"),
            _ = ping.tick() => {
                socket.send(Message::text(json!({"method": "ping"}).to_string())).await?;
            }
            received = socket.next() => match received {
                Some(Ok(Message::Text(text))) => {
                    idle.as_mut().reset(Instant::now() + IDLE_LIMIT);
                    let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
                        warn!("dropped a message that is not valid JSON");
                        continue;
                    };
                    strip_users(&mut value);
                    if value["channel"] == "error" {
                        warn!(%value, "server reported an error");
                    }
                    serde_json::to_writer(&mut *out, &value)?;
                    out.write_all(b"\n")?;
                    lines += 1;
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => bail!("connection lost after {lines} lines: {err}"),
                None => bail!("connection closed after {lines} lines"),
            },
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let path = match std::env::var("RECORD_PATH") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            PathBuf::from(format!("data/hyperliquid-btc-{secs}.jsonl"))
        }
    };
    let limit = match std::env::var("RECORD_SECONDS") {
        Ok(value) => {
            let seconds = value.parse::<u64>().ok().filter(|seconds| *seconds > 0);
            Some(Duration::from_secs(
                seconds.context("RECORD_SECONDS must be an integer above zero")?,
            ))
        }
        Err(_) => None,
    };

    let (mut socket, _) = connect_async(URL)
        .await
        .with_context(|| format!("cannot connect to {URL}"))?;
    let subscriptions = [
        json!({"type": "l2Book", "coin": "BTC"}),
        json!({"type": "l2Book", "coin": "BTC", "fast": true}),
        json!({"type": "trades", "coin": "BTC"}),
    ];
    for subscription in subscriptions {
        let request = json!({"method": "subscribe", "subscription": subscription});
        socket.send(Message::text(request.to_string())).await?;
    }

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let file = File::create_new(&path)
        .with_context(|| format!("cannot create {} (it must not exist yet)", path.display()))?;
    let mut out = BufWriter::new(file);
    info!(path = %path.display(), "recording");

    let result = record(&mut socket, &mut out, limit).await;
    let flushed = out.flush().context("cannot flush the recording");
    result?;
    flushed?;
    info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stripped(text: &str) -> Value {
        let mut value: Value = serde_json::from_str(text).expect("test input is valid JSON");
        strip_users(&mut value);
        value
    }

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).expect("test input is valid JSON")
    }

    #[test]
    fn users_are_removed_from_an_array_of_trades() {
        let input = r#"{"channel":"trades","data":[
            {"coin":"BTC","px":"1","tid":4,"users":["a","b"]},
            {"coin":"BTC","px":"5","tid":8,"users":["c","d"]}]}"#;
        assert_eq!(
            stripped(input),
            parsed(
                r#"{"channel":"trades","data":[
                {"coin":"BTC","px":"1","tid":4},{"coin":"BTC","px":"5","tid":8}]}"#
            )
        );
    }

    #[test]
    fn users_are_removed_from_an_object_payload_and_other_channels() {
        let input = r#"{"channel":"other","data":{"users":["a"],"kept":1}}"#;
        assert_eq!(
            stripped(input),
            parsed(r#"{"channel":"other","data":{"kept":1}}"#)
        );
    }

    #[test]
    fn users_are_removed_at_any_depth() {
        let input = r#"{"data":[{"deep":{"list":[{"users":["a"],"kept":true}]}}],"users":[]}"#;
        assert_eq!(
            stripped(input),
            parsed(r#"{"data":[{"deep":{"list":[{"kept":true}]}}]}"#)
        );
    }

    #[test]
    fn messages_without_users_are_unchanged() {
        let input = r#"{"channel":"l2Book","data":{"coin":"BTC","levels":[[{"px":"1","sz":"2","n":3}],[]]}}"#;
        assert_eq!(stripped(input), parsed(input));
    }
}
