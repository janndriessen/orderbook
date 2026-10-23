//! The `record` binary: writes the live BTC book and trades from Hyperliquid's
//! public WebSocket to a JSON Lines file, one received message per line.
//!
//! The main connection subscribes to three documented book feeds: the standard
//! one, 20 levels per side about every 5 seconds, the fast one, 5 levels per
//! side about every half second, and the best bid and offer, one level per
//! side, sent whenever either changes (about every 0.1 seconds). The two book
//! feeds have the same shape and only their depth tells them apart; the best
//! bid and offer has its own channel. Trades come on the main connection too.
//!
//! The `l2` feed, which Hyperliquid's web app uses and its API documentation
//! does not list, sends 20 levels per side about every half second, at one
//! price grouping per subscription. All six groupings the app offers for BTC
//! are recorded. No `l2` message says which grouping it is for, so each one
//! gets its own connection (six in all, within the limit of ten per address on
//! <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits>),
//! and the recorder adds a `subscription` field to every `l2` line: the
//! subscription it sent on that connection. An `l2` message that already has
//! such a field ends the run rather than lose what the source sent.
//!
//! Every message is parsed and written back re-serialized (keys sorted), with
//! any `users` field, which carries wallet addresses, removed at every depth.
//! The run ends with an error, and there is no reconnect because a gap in a
//! recording would be silent, when:
//!
//! - any connection is closed or lost;
//! - a text message is not valid JSON, which is never written;
//! - no full book snapshot (more than 5 levels on a side) arrives on the main
//!   connection for 30 seconds, which catches a stalled standard feed that the
//!   fast feed would otherwise hide, and a silent main connection with it;
//! - no `l2` message arrives on one of the six connections for 30 seconds.
//!
//! Acknowledgements, `pong` replies and `error` messages count as no progress.
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
use futures_util::stream::{self, SplitSink, select_all};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, interval_at, sleep, sleep_until};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const URL: &str = "wss://api.hyperliquid.xyz/ws";

/// How long a feed may stay silent before its connection counts as dead: the
/// full book snapshots come about every 5 seconds, `l2` messages about every
/// half second.
const IDLE_LIMIT: Duration = Duration::from_secs(30);

/// How often the documented application-level ping is sent, so that an idle
/// rule on the server side cannot close the connection.
const PING_EVERY: Duration = Duration::from_secs(30);

/// The most levels per side that a top-of-book snapshot carries.
const TOP_DEPTH: usize = 5;

/// The `l2` price groupings the web app offers for BTC, finest first, as
/// significant figures and an optional mantissa: $1, $2, $5, $10, $100 and
/// $1,000 at current prices. Connection `i` carries grouping `i`.
const GROUPINGS: [(u8, Option<u8>); 6] = [
    (5, None),
    (5, Some(2)),
    (5, Some(5)),
    (4, None),
    (3, None),
    (2, None),
];

/// The `l2` subscription for one grouping, as sent and as written into its
/// lines.
fn l2_subscription(grouping: usize) -> Value {
    let (figures, mantissa) = GROUPINGS[grouping];
    let mut subscription = json!({"type": "l2", "c": "BTC", "s": figures});
    if let Some(mantissa) = mantissa {
        subscription["m"] = json!(mantissa);
    }
    subscription
}

/// What connection `index` subscribes to: the main connection takes the
/// documented feeds and trades as well as the finest `l2` grouping.
fn subscriptions(index: usize) -> Vec<Value> {
    let mut subscriptions = Vec::new();
    if index == 0 {
        subscriptions.extend([
            json!({"type": "l2Book", "coin": "BTC"}),
            json!({"type": "l2Book", "coin": "BTC", "fast": true}),
            json!({"type": "bbo", "coin": "BTC"}),
            json!({"type": "trades", "coin": "BTC"}),
        ]);
    }
    subscriptions.push(l2_subscription(index));
    subscriptions
}

/// What a received message is, as far as progress is concerned.
#[derive(Debug, PartialEq, Eq)]
enum Received {
    /// A book snapshot deeper than the fast feed's.
    FullBook,
    /// An `l2` message: a full book or a change to it.
    L2,
    /// Anything else: top-of-book snapshots, the best bid and offer and
    /// trades, which no deadline waits for, and acknowledgements, `pong` and
    /// `error` messages, which are no progress.
    Other,
}

fn classify(value: &Value) -> Received {
    match value["channel"].as_str() {
        Some("l2Book") => {
            let depth = |side: usize| value["data"]["levels"][side].as_array().map_or(0, Vec::len);
            if depth(0).max(depth(1)) > TOP_DEPTH {
                Received::FullBook
            } else {
                Received::Other
            }
        }
        Some("l2") => Received::L2,
        _ => Received::Other,
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// When each kind of silence would end the run.
#[derive(Debug)]
struct Deadlines {
    /// The next full book snapshot on the main connection.
    full_book: Instant,
    /// The next `l2` message on each connection.
    l2: [Instant; GROUPINGS.len()],
}

impl Deadlines {
    fn new(now: Instant) -> Self {
        Self {
            full_book: now + IDLE_LIMIT,
            l2: [now + IDLE_LIMIT; GROUPINGS.len()],
        }
    }

    /// Moves the deadlines that a message of this kind on this connection
    /// meets.
    fn note(&mut self, connection: usize, received: &Received, now: Instant) {
        if connection == 0 && *received == Received::FullBook {
            self.full_book = now + IDLE_LIMIT;
        }
        if *received == Received::L2 {
            self.l2[connection] = now + IDLE_LIMIT;
        }
    }

    /// The earliest deadline.
    fn next(&self) -> Instant {
        self.l2.iter().copied().fold(self.full_book, Instant::min)
    }

    /// What has gone silent for too long at `now`, if anything.
    fn missed(&self, now: Instant) -> Option<String> {
        if now >= self.full_book {
            return Some("no full book snapshot".to_owned());
        }
        let grouping = self.l2.iter().position(|deadline| now >= *deadline)?;
        Some(format!("no l2 message for {}", l2_subscription(grouping)))
    }
}

/// Adds the connection's `l2` subscription to an `l2` message, the only place
/// that says which grouping the message is for. A message that already has
/// the field is an error, since writing over it would lose source data.
fn tag(value: &mut Value, connection: usize) -> anyhow::Result<()> {
    if value["channel"] != "l2" {
        return Ok(());
    }
    if let Some(map) = value.as_object_mut() {
        if map.contains_key("subscription") {
            bail!("an l2 message on connection {connection} already has a subscription field");
        }
        map.insert("subscription".to_owned(), l2_subscription(connection));
    }
    Ok(())
}

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

/// Reads messages from every connection into `out` until the time limit or
/// Ctrl-C; any other end is an error.
async fn record(
    sockets: [Socket; GROUPINGS.len()],
    out: &mut BufWriter<File>,
    limit: Option<Duration>,
) -> anyhow::Result<()> {
    let (mut sinks, streams): (Vec<SplitSink<Socket, Message>>, Vec<_>) =
        sockets.into_iter().map(StreamExt::split).unzip();
    // Each stream ends with `None`, so a closed connection is seen by index
    // rather than just leaving the merged stream.
    let mut received = select_all(streams.into_iter().enumerate().map(|(index, socket)| {
        socket
            .map(move |message| (index, Some(message)))
            .chain(stream::once(async move { (index, None) }))
            .boxed()
    }));
    let stop = sleep(limit.unwrap_or(Duration::MAX));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(stop, ctrl_c);
    let mut deadlines = Deadlines::new(Instant::now());
    let mut ping = interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let mut lines = 0u64;
    loop {
        tokio::select! {
            () = &mut stop => return Ok(()),
            signal = &mut ctrl_c => {
                signal.context("cannot listen for ctrl-c")?;
                return Ok(());
            }
            () = sleep_until(deadlines.next()) => {
                if let Some(missed) = deadlines.missed(Instant::now()) {
                    bail!("{missed} for {IDLE_LIMIT:?} after {lines} lines");
                }
            }
            _ = ping.tick() => {
                for (connection, sink) in sinks.iter_mut().enumerate() {
                    sink.send(Message::text(json!({"method": "ping"}).to_string()))
                        .await
                        .with_context(|| {
                            format!("cannot ping connection {connection} after {lines} lines")
                        })?;
                }
            }
            Some((connection, message)) = received.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let mut value = serde_json::from_str::<Value>(&text)
                        .with_context(|| format!("message after {lines} lines is not valid JSON"))?;
                    deadlines.note(connection, &classify(&value), Instant::now());
                    strip_users(&mut value);
                    tag(&mut value, connection)?;
                    if value["channel"] == "error" {
                        warn!(%value, connection, "server reported an error");
                    }
                    serde_json::to_writer(&mut *out, &value)?;
                    out.write_all(b"\n")?;
                    lines += 1;
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    bail!("connection {connection} lost after {lines} lines: {err}")
                }
                None => bail!("connection {connection} closed after {lines} lines"),
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

    let mut sockets = Vec::new();
    for index in 0..GROUPINGS.len() {
        let (mut socket, _) = connect_async(URL)
            .await
            .with_context(|| format!("cannot connect to {URL}"))?;
        for subscription in subscriptions(index) {
            let request = json!({"method": "subscribe", "subscription": subscription});
            socket.send(Message::text(request.to_string())).await?;
        }
        sockets.push(socket);
    }
    let sockets = <[Socket; GROUPINGS.len()]>::try_from(sockets)
        .map_err(|_| anyhow::anyhow!("one connection per grouping was not opened"))?;

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let file = File::create_new(&path)
        .with_context(|| format!("cannot create {} (it must not exist yet)", path.display()))?;
    let mut out = BufWriter::new(file);
    info!(path = %path.display(), "recording");

    let result = record(sockets, &mut out, limit).await;
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
    fn messages_are_classified_by_channel_and_book_depth() {
        let book = |depth: usize, other: usize| {
            let side = |n: usize| vec![json!({"px": "1", "sz": "1", "n": 1}); n];
            json!({"channel": "l2Book", "data": {"coin": "BTC", "levels": [side(depth), side(other)]}})
        };
        assert_eq!(classify(&book(20, 20)), Received::FullBook);
        assert_eq!(classify(&book(5, 6)), Received::FullBook);
        assert_eq!(classify(&book(5, 5)), Received::Other);
        assert_eq!(classify(&book(0, 0)), Received::Other);
        assert_eq!(
            classify(&json!({"channel": "l2", "data": {"c": "abc"}})),
            Received::L2
        );
        for other in [
            json!({"channel": "trades", "data": []}),
            json!({"channel": "bbo", "data": {"coin": "BTC", "bbo": [null, null]}}),
            json!({"channel": "pong"}),
            json!({"channel": "subscriptionResponse", "data": {}}),
            json!({"channel": "error", "data": "x"}),
            json!({}),
        ] {
            assert_eq!(classify(&other), Received::Other);
        }
    }

    #[test]
    fn each_connection_subscribes_to_one_grouping_and_the_main_one_to_more() {
        let main = subscriptions(0);
        assert_eq!(main.len(), 5);
        assert_eq!(main[4], json!({"type": "l2", "c": "BTC", "s": 5}));
        let rest: Vec<Vec<Value>> = (1..GROUPINGS.len()).map(subscriptions).collect();
        assert_eq!(
            rest,
            vec![
                vec![json!({"type": "l2", "c": "BTC", "s": 5, "m": 2})],
                vec![json!({"type": "l2", "c": "BTC", "s": 5, "m": 5})],
                vec![json!({"type": "l2", "c": "BTC", "s": 4})],
                vec![json!({"type": "l2", "c": "BTC", "s": 3})],
                vec![json!({"type": "l2", "c": "BTC", "s": 2})],
            ]
        );
    }

    #[test]
    fn l2_messages_are_tagged_with_their_connections_subscription() {
        let mut change = json!({"channel": "l2", "data": {"c": "abc"}});
        tag(&mut change, 3).expect("tags");
        assert_eq!(
            change,
            json!({"channel": "l2", "data": {"c": "abc"}, "subscription": {"type": "l2", "c": "BTC", "s": 4}})
        );
        assert_eq!(classify(&change), Received::L2);
        for other in [
            json!({"channel": "trades", "data": []}),
            json!({"channel": "subscriptionResponse", "data": {"subscription": {"type": "l2"}}}),
            json!({"channel": "pong"}),
        ] {
            let mut tagged = other.clone();
            tag(&mut tagged, 1).expect("leaves other channels alone");
            assert_eq!(tagged, other);
        }
        let mut already = json!({"channel": "l2", "data": {}, "subscription": "source"});
        assert!(tag(&mut already, 0).is_err());
        assert_eq!(already["subscription"], "source");
    }

    /// Opens one local connection per grouping, of which the server closes
    /// `closed` right away and keeps the others open and silent.
    async fn local_connections(closed: usize) -> [Socket; GROUPINGS.len()] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a local port");
        let addr = listener.local_addr().expect("has an address");
        tokio::spawn(async move {
            let mut open = Vec::new();
            for index in 0..GROUPINGS.len() {
                let (tcp, _) = listener.accept().await.expect("accepts");
                let mut socket = tokio_tungstenite::accept_async(tcp)
                    .await
                    .expect("handshakes");
                if index == closed {
                    let _ = socket.close(None).await;
                } else {
                    open.push(socket);
                }
            }
            std::future::pending::<()>().await;
        });
        let mut sockets = Vec::new();
        for _ in 0..GROUPINGS.len() {
            let (socket, _) = connect_async(format!("ws://{addr}"))
                .await
                .expect("connects");
            sockets.push(socket);
        }
        <[Socket; GROUPINGS.len()]>::try_from(sockets).unwrap_or_else(|_| panic!("six sockets"))
    }

    #[tokio::test]
    async fn a_closed_connection_ends_the_run_and_is_named() {
        let path = std::env::temp_dir().join(format!("record-test-{}.jsonl", std::process::id()));
        let mut out = BufWriter::new(File::create(&path).expect("creates a temp file"));
        let sockets = local_connections(3).await;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            record(sockets, &mut out, Some(Duration::from_secs(60))),
        )
        .await
        .expect("ends well before the idle limit");
        drop(out);
        let _ = fs::remove_file(&path);
        let message = result
            .expect_err("a closed connection is an error")
            .to_string();
        assert!(
            message.starts_with("connection 3 closed") || message.starts_with("connection 3 lost"),
            "{message}"
        );
    }

    #[test]
    fn each_connection_has_its_own_l2_deadline() {
        let start = Instant::now();
        let mut deadlines = Deadlines::new(start);
        let later = start + IDLE_LIMIT / 2;
        // Every connection but number 2 sends l2, and the main one sends a
        // full book; number 2 still runs out at the first limit.
        for connection in [0, 1, 3, 4, 5] {
            deadlines.note(connection, &Received::L2, later);
        }
        deadlines.note(0, &Received::FullBook, later);
        assert_eq!(deadlines.next(), start + IDLE_LIMIT);
        assert_eq!(
            deadlines.missed(start + IDLE_LIMIT - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            deadlines.missed(start + IDLE_LIMIT).as_deref(),
            Some(r#"no l2 message for {"c":"BTC","m":5,"s":5,"type":"l2"}"#)
        );
        deadlines.note(2, &Received::L2, later);
        assert_eq!(deadlines.missed(start + IDLE_LIMIT), None);
        assert_eq!(deadlines.next(), later + IDLE_LIMIT);
    }

    #[test]
    fn only_a_full_book_on_the_main_connection_keeps_its_deadline_alive() {
        let start = Instant::now();
        let mut deadlines = Deadlines::new(start);
        let later = start + IDLE_LIMIT / 2;
        for connection in 0..GROUPINGS.len() {
            deadlines.note(connection, &Received::L2, later);
        }
        for connection in 1..GROUPINGS.len() {
            deadlines.note(connection, &Received::FullBook, later);
        }
        deadlines.note(0, &Received::Other, later);
        // The full book is now the only deadline left at the first limit, so
        // the loop must wake for it even while every l2 feed is flowing.
        assert_eq!(deadlines.next(), start + IDLE_LIMIT);
        assert_eq!(
            deadlines.missed(start + IDLE_LIMIT).as_deref(),
            Some("no full book snapshot")
        );
        deadlines.note(0, &Received::FullBook, later);
        assert_eq!(deadlines.missed(start + IDLE_LIMIT), None);
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
