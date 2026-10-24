//! Runs the live path against a local WebSocket server that stands in for the
//! source: it answers each connection's subscriptions with the lines of the
//! recorded fixture that belong to it, and the book the service ends up
//! serving is compared with what `load` makes of the same recording.
//!
//! The server adds one more full book per grouping after the recording, the
//! same as the last one, as a source that keeps sending would; the expected
//! book is the one `load` makes of the recording with those lines added.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hyperliquid_adapter::{BookLevel, BookSnapshot, L2Decoder, Message};
use hyperliquid_recorder::CONNECTIONS;
use matching::Matcher;
use protocol::{Command, FeedMessage, Level, Price, Side};
use replayer::live::{Retry, serve};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::{WebSocketStream, accept_async, connect_async};

const FIXTURE: &str = include_str!("fixtures/hyperliquid-btc-l2-10s.jsonl");

/// How long any single wait of a test may take before it fails instead of
/// hanging.
const WAIT: Duration = Duration::from_secs(10);

/// A pause between connection attempts that keeps the reconnect test quick.
const QUICK: Retry = Retry {
    initial: Duration::from_millis(20),
    max: Duration::from_millis(100),
};

type Levels = BTreeMap<Price, u64>;
type Socket = WebSocketStream<TcpStream>;

/// A grouping as its subscription names it: `s` and `m`, both absent for the
/// grouping without rounding.
type Key = (Option<u64>, Option<u64>);

/// The grouping the fixture's `s: 5` lines are, which the main connection
/// carries.
const MAIN: Key = (Some(5), None);

/// The grouping without rounding, which the fixture does not have: below
/// $100,000 it shows exactly what `s: 5` does, so it is served those lines.
const EXACT: Key = (None, None);

/// A line of the source, ready to send, with the push time of its book in
/// microseconds.
struct Sent {
    ts: u64,
    text: String,
}

/// The fixture, read once: what each grouping sent, the books it showed, and
/// the trades.
struct Recording {
    lines: BTreeMap<Key, Vec<Sent>>,
    books: BTreeMap<Key, Vec<BookSnapshot>>,
    trades: Vec<Sent>,
}

fn recording() -> Recording {
    let market = engine::market();
    let mut decoders: BTreeMap<Key, L2Decoder> = BTreeMap::new();
    let mut lines: BTreeMap<Key, Vec<Sent>> = BTreeMap::new();
    let mut books: BTreeMap<Key, Vec<BookSnapshot>> = BTreeMap::new();
    let mut trades = Vec::new();
    for text in FIXTURE.lines() {
        let mut value: Value = serde_json::from_str(text).expect("fixture line");
        match value["channel"].as_str() {
            Some("l2") => {
                let subscription = value["subscription"].clone();
                let key = (subscription["s"].as_u64(), subscription["m"].as_u64());
                // The source does not tag its lines; the recorder did.
                value
                    .as_object_mut()
                    .expect("object")
                    .remove("subscription");
                let Message::L2(wire) = serde_json::from_value(value.clone()).expect("l2") else {
                    panic!("not an l2 message");
                };
                let book = decoders
                    .entry(key)
                    .or_default()
                    .apply(&wire, &market)
                    .expect("applies")
                    .clone();
                lines.entry(key).or_default().push(Sent {
                    ts: book.ts,
                    text: value.to_string(),
                });
                books.entry(key).or_default().push(book);
            }
            Some("trades") => {
                let ms = value["data"][0]["time"].as_u64().expect("trade time");
                trades.push(Sent {
                    ts: ms * 1_000,
                    text: value.to_string(),
                });
            }
            _ => {}
        }
    }
    Recording {
        lines,
        books,
        trades,
    }
}

/// A full book as the source sends it on subscribing.
fn full_book(book: &BookSnapshot, ts: u64) -> Sent {
    let side = |levels: &[BookLevel]| -> Vec<Value> {
        levels
            .iter()
            .map(|level| {
                json!({
                    "px": format!("{}.0", level.price),
                    "sz": format!("{}.{:05}", level.qty / 100_000, level.qty % 100_000),
                    "n": level.orders,
                })
            })
            .collect()
    };
    let text = json!({
        "channel": "l2",
        "data": {"s": {
            "coin": "BTC",
            "time": ts / 1_000,
            "levels": [side(&book.bids), side(&book.asks)],
        }},
    })
    .to_string();
    Sent { ts, text }
}

/// What the fake source sends in one round: per grouping the lines, and the
/// trades for the main connection.
struct Round {
    lines: BTreeMap<Key, Vec<Sent>>,
    trades: Vec<Sent>,
}

/// The lines strictly between the two times (`None` for no bound).
fn copied(sent: &[Sent], after: Option<u64>, before: Option<u64>) -> Vec<Sent> {
    sent.iter()
        .filter(|s| after.is_none_or(|a| s.ts > a) && before.is_none_or(|b| s.ts < b))
        .map(|s| Sent {
            ts: s.ts,
            text: s.text.clone(),
        })
        .collect()
}

/// The last full book of every grouping, half a second after the last line of
/// the recording.
fn added_books(fixture: &Recording) -> BTreeMap<Key, Sent> {
    let end = fixture
        .lines
        .values()
        .filter_map(|lines| lines.last())
        .map(|s| s.ts)
        .max()
        .expect("lines");
    fixture
        .books
        .iter()
        .map(|(key, books)| {
            let last = books.last().expect("a book");
            (*key, full_book(last, end + 500_000))
        })
        .collect()
}

/// The fixture as one round, with the last full book of every grouping added
/// after it.
fn whole_round(fixture: &Recording) -> Round {
    let mut added = added_books(fixture);
    let mut lines = BTreeMap::new();
    for (key, sent) in &fixture.lines {
        let mut all = copied(sent, None, None);
        all.push(added.remove(key).expect("an added book"));
        lines.insert(*key, all);
    }
    Round {
        lines,
        trades: copied(&fixture.trades, None, None),
    }
}

/// Pushes at which every grouping of the fixture sent a book.
fn complete_pushes(fixture: &Recording) -> Vec<u64> {
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    for books in fixture.books.values() {
        for book in books {
            *counts.entry(book.ts).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter(|&(_, count)| count == fixture.books.len())
        .map(|(ts, _)| ts)
        .collect()
}

/// The fixture cut at the complete push `split`: a first round of everything
/// before it, and a second that starts as new subscriptions do, with a full
/// book of the push, and goes on with what follows. The last full books come
/// after the end as in [`whole_round`].
fn split_rounds(fixture: &Recording, split: u64) -> Vec<Round> {
    let mut added = added_books(fixture);
    let mut first = BTreeMap::new();
    let mut second = BTreeMap::new();
    for (key, sent) in &fixture.lines {
        // The first round ends with a full book of its own, 1 ms before the
        // cut, as a source that keeps sending would.
        let before = fixture.books[key]
            .iter()
            .rev()
            .find(|book| book.ts < split)
            .expect("a book before the cut");
        let mut lines = copied(sent, None, Some(split));
        let last = lines.last().expect("lines before the cut").ts;
        assert!(last < split - 1_000, "the cut leaves room for the book");
        lines.push(full_book(before, split - 1_000));
        first.insert(*key, lines);
        let book_at_split = fixture.books[key]
            .iter()
            .find(|book| book.ts == split)
            .expect("every grouping has a book at a complete push");
        let mut rest = vec![full_book(book_at_split, split)];
        rest.extend(copied(sent, Some(split), None));
        rest.push(added.remove(key).expect("an added book"));
        second.insert(*key, rest);
    }
    vec![
        Round {
            lines: first,
            trades: copied(&fixture.trades, None, Some(split)),
        },
        Round {
            lines: second,
            trades: copied(&fixture.trades, Some(split), None),
        },
    ]
}

/// What a connection asked for: its grouping and whether it is the main one.
async fn subscribed(socket: &mut Socket) -> (Key, bool) {
    let mut main = false;
    let mut key = None;
    let mut expected = 1;
    while expected > 0 {
        let frame = timeout(WAIT, socket.next())
            .await
            .expect("the client subscribes")
            .expect("the connection stays open")
            .expect("a frame");
        let Frame::Text(text) = frame else { continue };
        let request: Value = serde_json::from_str(text.as_str()).expect("request");
        let subscription = &request["subscription"];
        expected -= 1;
        if subscription["type"] == "l2" {
            key = Some((subscription["s"].as_u64(), subscription["m"].as_u64()));
        } else if !main {
            // The main connection sends the documented feeds first and its
            // `l2` subscription last: four more messages follow this one.
            main = true;
            expected += 4;
        }
    }
    (key.expect("an l2 subscription"), main)
}

/// The lines the main connection gets: its grouping's lines and the trades in
/// the order of their times, a trade before a book of the same time.
fn with_trades(lines: &[Sent], trades: &[Sent]) -> Vec<String> {
    let mut merged: Vec<(u64, u8, &str)> = trades
        .iter()
        .map(|t| (t.ts, 0, t.text.as_str()))
        .chain(lines.iter().map(|l| (l.ts, 1, l.text.as_str())))
        .collect();
    merged.sort_by_key(|&(ts, order, _)| (ts, order));
    merged
        .into_iter()
        .map(|(_, _, text)| text.to_owned())
        .collect()
}

/// Serves the rounds one after another: each round accepts one connection
/// per grouping, sends each its lines, and, unless it is the last round,
/// closes them all. Sends the number of connections served once the last
/// round has sent everything and keeps its connections open.
fn fake_source(
    listener: TcpListener,
    rounds: Vec<Round>,
    served: mpsc::UnboundedSender<usize>,
    mut gate: mpsc::UnboundedReceiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let count = rounds.len();
        let mut connections = 0;
        let mut held = Vec::new();
        for (index, round) in rounds.into_iter().enumerate() {
            let round = Arc::new(round);
            let mut senders = Vec::new();
            for _ in 0..CONNECTIONS {
                let (tcp, _) = listener.accept().await.expect("accepts");
                let mut socket = accept_async(tcp).await.expect("handshakes");
                connections += 1;
                let round = Arc::clone(&round);
                // Each connection sends on its own, so that one whose buffer
                // fills up until the client reads cannot hold up accepting
                // the next.
                senders.push(tokio::spawn(async move {
                    let (key, main) = subscribed(&mut socket).await;
                    // The grouping without rounding is served the `s: 5`
                    // lines.
                    let from = if key == EXACT { MAIN } else { key };
                    let lines = if main {
                        with_trades(&round.lines[&from], &round.trades)
                    } else {
                        round.lines[&from].iter().map(|l| l.text.clone()).collect()
                    };
                    for line in lines {
                        socket.send(Frame::text(line)).await.expect("sends");
                    }
                    socket
                }));
            }
            let mut sockets = Vec::new();
            for sender in senders {
                sockets.push(sender.await.expect("sender task"));
            }
            if index + 1 < count {
                // Closed only when the client has the round's book.
                timeout(3 * WAIT, gate.recv())
                    .await
                    .expect("the client got the round's book")
                    .expect("the test is listening");
                for mut socket in sockets {
                    let _ = socket.close(None).await;
                }
            } else {
                held = sockets;
            }
        }
        served.send(connections).expect("test is listening");
        // Keep the last sockets open until the test aborts this task.
        let _held = held;
        std::future::pending::<()>().await;
    })
}

/// The levels of the matcher after all commands, per side, as price to total.
fn matcher_book(commands: &[(u64, Command)]) -> (Levels, Levels) {
    let mut matcher = Matcher::new();
    for (_, command) in commands {
        matcher.apply(*command);
    }
    let levels = |side| {
        matcher
            .book()
            .levels(side)
            .map(|(price, level)| (price, level.total()))
            .collect()
    };
    (levels(Side::Buy), levels(Side::Sell))
}

fn patch(side: &mut Levels, changes: &[Level]) {
    for Level { price, qty } in changes {
        if *qty == 0 {
            side.remove(price);
        } else {
            side.insert(*price, *qty);
        }
    }
}

/// The book `load` gives for a round written out as a recording: every line
/// tagged with its grouping, in the order of their times.
fn expected_of(round: &Round) -> (Levels, Levels) {
    let mut lines: Vec<(u64, u8, String)> = round
        .trades
        .iter()
        .map(|t| (t.ts, 0, t.text.clone()))
        .collect();
    for (key, sent) in &round.lines {
        let mut subscription = json!({"c": "BTC", "type": "l2"});
        if let Some(s) = key.0 {
            subscription["s"] = json!(s);
        }
        if let Some(m) = key.1 {
            subscription["m"] = json!(m);
        }
        for line in sent {
            let mut value: Value = serde_json::from_str(&line.text).expect("line");
            value["subscription"] = subscription.clone();
            lines.push((line.ts, 1, value.to_string()));
        }
    }
    lines.sort_by_key(|&(ts, order, _)| (ts, order));
    let text: String = lines
        .into_iter()
        .map(|(_, _, line)| format!("{line}\n"))
        .collect();
    let loaded = replayer::load(&text, &engine::market()).expect("loads");
    matcher_book(&loaded.commands)
}

/// Starts the service on the live path against a fake source that plays
/// `rounds`, follows its feed as a client until the client's book equals
/// `expected`, then shuts the service down and checks that it stops. A
/// source that plays several rounds closes its connections after each but the
/// last only once the client's book equals `checkpoint`, so that the
/// connections are lost with the book of the round already in the engine.
/// The lines received are also given to `record`. Returns how many
/// connections the source accepted and how long the book took to reach
/// `expected`.
async fn follow(
    rounds: Vec<Round>,
    checkpoint: Option<(Levels, Levels)>,
    expected: (Levels, Levels),
    record: Option<Box<dyn Write + Send>>,
) -> (usize, Duration) {
    assert!(!expected.0.is_empty() && !expected.1.is_empty());
    let source = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", source.local_addr().expect("addr"));
    let (served, mut served_rx) = mpsc::unbounded_channel();
    let (open_gate, gate) = mpsc::unbounded_channel();
    let fake = fake_source(source, rounds, served, gate);
    let mut checkpoint = checkpoint;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let shutdown = engine::CancellationToken::new();
    let service = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { serve(listener, &url, record, shutdown, QUICK).await }
    });
    let started = Instant::now();
    let (mut client, _) = timeout(WAIT, connect_async(format!("ws://{addr}/ws")))
        .await
        .expect("the service accepts the client")
        .expect("connect");

    let (mut bids, mut asks) = (Levels::new(), Levels::new());
    let mut seq = None;
    let deadline = Instant::now() + 3 * WAIT;
    loop {
        let frame = timeout_at(deadline, client.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the book never reached the expected one (checkpoint still pending: {}; \
                     {} bid and {} ask levels, expected {} and {})",
                    checkpoint.is_some(),
                    bids.len(),
                    asks.len(),
                    expected.0.len(),
                    expected.1.len()
                )
            })
            .expect("the service keeps the feed open")
            .expect("a frame");
        let Frame::Text(text) = frame else { continue };
        match serde_json::from_str(text.as_str()).expect("feed message") {
            FeedMessage::Snapshot {
                seq: first,
                bids: b,
                asks: a,
                ..
            } => {
                seq = Some(first);
                bids = b.iter().map(|l| (l.price, l.qty)).collect();
                asks = a.iter().map(|l| (l.price, l.qty)).collect();
            }
            FeedMessage::Update {
                seq: next,
                bids: b,
                asks: a,
                ..
            } => {
                assert_eq!(Some(next), seq.map(|s| s + 1), "gap in the feed");
                seq = Some(next);
                patch(&mut bids, &b);
                patch(&mut asks, &a);
            }
            FeedMessage::Heartbeat { .. } => {}
        }
        if checkpoint
            .as_ref()
            .is_some_and(|c| (&bids, &asks) == (&c.0, &c.1))
        {
            checkpoint = None;
            open_gate.send(()).expect("the source is waiting");
        }
        if checkpoint.is_none() && (&bids, &asks) == (&expected.0, &expected.1) {
            break;
        }
    }
    let took = started.elapsed();
    let connections = timeout(WAIT, served_rx.recv())
        .await
        .expect("the source finished")
        .expect("connections counted");

    shutdown.cancel();
    timeout(WAIT, service)
        .await
        .expect("the service did not stop")
        .expect("service task")
        .expect("service result");
    fake.abort();
    (connections, took)
}

#[tokio::test]
async fn the_book_ends_as_the_recording_does() {
    let fixture = recording();
    let round = whole_round(&fixture);
    let expected = expected_of(&round);
    let (connections, _) = follow(vec![round], None, expected, None).await;
    assert_eq!(connections, CONNECTIONS);
}

#[tokio::test]
async fn the_book_ends_right_after_the_source_drops_every_connection_once() {
    let fixture = recording();
    let pushes = complete_pushes(&fixture);
    assert!(pushes.len() >= 10, "the fixture has complete pushes");
    // Half way through the complete pushes.
    let split = pushes[pushes.len() / 2];
    let expected = expected_of(&whole_round(&fixture));
    let rounds = split_rounds(&fixture, split);
    let checkpoint = expected_of(&rounds[0]);
    assert_ne!(checkpoint, expected, "the book moves after the cut");
    let (connections, _) = follow(rounds, Some(checkpoint), expected, None).await;
    assert_eq!(
        connections,
        2 * CONNECTIONS,
        "every connection was reopened"
    );
}

/// A recording writer that takes its time on every line.
struct Slow;

impl Write for Slow {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        std::thread::sleep(Duration::from_millis(10));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A recording writer that fails, or panics, on its first line.
struct Broken {
    panic: bool,
}

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        assert!(!self.panic, "the disk caught fire");
        Err(io::Error::other("disk full"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_slow_recording_does_not_hold_up_the_book() {
    // The runtime of a test has one thread: a write on the connections task
    // would stall everything for 20 ms per line, some 3 s for the fixture.
    let fixture = recording();
    let round = whole_round(&fixture);
    let expected = expected_of(&round);
    let (_, took) = follow(vec![round], None, expected, Some(Box::new(Slow))).await;
    assert!(took < Duration::from_millis(1500), "the book took {took:?}");
}

/// Serves the fixture to a service whose recording writer is `broken`, and
/// returns what `serve` returns, which it must do on its own.
async fn serve_with_broken_recording(broken: Broken) -> (io::Result<()>, bool) {
    let fixture = recording();
    let source = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", source.local_addr().expect("addr"));
    let (served, _served_rx) = mpsc::unbounded_channel();
    let (_open_gate, gate) = mpsc::unbounded_channel();
    let fake = fake_source(source, vec![whole_round(&fixture)], served, gate);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let shutdown = engine::CancellationToken::new();
    let result = timeout(
        WAIT,
        serve(
            listener,
            &url,
            Some(Box::new(broken)),
            shutdown.clone(),
            QUICK,
        ),
    )
    .await
    .expect("the service did not stop on its own");
    fake.abort();
    (result, shutdown.is_cancelled())
}

#[tokio::test]
async fn a_recording_that_cannot_be_written_stops_the_service_with_its_error() {
    let (result, cancelled) = serve_with_broken_recording(Broken { panic: false }).await;
    let err = result.expect_err("the service reports the failure");
    assert!(err.to_string().contains("disk full"), "{err}");
    assert!(cancelled);
}

#[tokio::test]
async fn a_recording_writer_that_panics_stops_the_service_with_an_error() {
    let (result, _) = serve_with_broken_recording(Broken { panic: true }).await;
    let err = result.expect_err("the service reports the failure");
    assert!(err.to_string().contains("panicked"), "{err}");
}

#[tokio::test]
async fn shutdown_completes_while_the_source_cannot_be_reached() {
    // A port nothing listens on: bound, then released.
    let unused = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", unused.local_addr().expect("addr"));
    drop(unused);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let shutdown = engine::CancellationToken::new();
    // A pause far longer than the test, so that the stop has to cut it short.
    let retry = Retry {
        initial: Duration::from_secs(3600),
        max: Duration::from_secs(3600),
    };
    let service = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { serve(listener, &url, None, shutdown, retry).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!service.is_finished(), "the service waits to reconnect");
    shutdown.cancel();
    timeout(WAIT, service)
        .await
        .expect("the service did not stop")
        .expect("service task")
        .expect("service result");
}

#[tokio::test]
async fn shutdown_completes_while_the_source_closes_every_connection() {
    let source = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", source.local_addr().expect("addr"));
    let closer = tokio::spawn(async move {
        loop {
            let (tcp, _) = source.accept().await.expect("accepts");
            let mut socket = accept_async(tcp).await.expect("handshakes");
            let _ = socket.close(None).await;
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let shutdown = engine::CancellationToken::new();
    let service = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { serve(listener, &url, None, shutdown, QUICK).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!service.is_finished(), "the service keeps trying");
    shutdown.cancel();
    timeout(WAIT, service)
        .await
        .expect("the service did not stop")
        .expect("service task")
        .expect("service result");
    closer.abort();
}
