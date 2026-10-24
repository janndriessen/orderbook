//! Connections to Hyperliquid's public WebSocket for the BTC book and trades,
//! yielding one JSON value per received message.
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
//! are subscribed, and a seventh: the subscription with no `s` at all, whose
//! buckets stay one tick wide at any price, where five figures turn $10 wide
//! from $100,000 on (the app uses it for its finest option there). No `l2`
//! message says which grouping it is for, so each one gets its own connection
//! (seven in all, within the limit of ten per address on
//! <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits>),
//! and every `l2` message gets a `subscription` field: the subscription sent
//! on that connection (without `s` for the seventh). An `l2` message that
//! already has such a field is an error rather than a loss of what the source
//! sent.
//!
//! Every message is parsed and handed out re-serialized (keys sorted) by
//! serde_json, with any `users` field, which carries wallet addresses,
//! removed at every depth. [`Connections::next`] fails, and there is no
//! reconnect because a gap would be silent, when:
//!
//! - any connection is closed or lost;
//! - a text message is not valid JSON, which is never handed out;
//! - no full book snapshot (more than 5 levels on a side) arrives on the main
//!   connection for 30 seconds, which catches a stalled standard feed that the
//!   fast feed would otherwise hide, and a silent main connection with it;
//! - no `l2` message arrives on one of the seven connections for 5 seconds.
//!
//! Acknowledgements, `pong` replies and `error` messages count as no progress.

use std::time::Duration;

use futures_util::stream::{self, BoxStream, SplitSink, select_all};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, Interval, interval_at, sleep_until};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::warn;

/// The address of Hyperliquid's public WebSocket.
pub const URL: &str = "wss://api.hyperliquid.xyz/ws";

/// How long the main connection may go without a full book snapshot before it
/// counts as dead: they come only about every 5 seconds.
const FULL_BOOK_LIMIT: Duration = Duration::from_secs(30);

/// How long a connection may go without an `l2` message before it counts as
/// dead. Measured over minutes, every connection sends one about every half
/// second (median 540 ms) and the longest gap on any of them, the coarsest
/// grouping included, was 625 ms, so this is eight times that. A live feed
/// that stops this long has stalled, and waiting longer only freezes the book.
const L2_LIMIT: Duration = Duration::from_secs(5);

/// How often the documented application-level ping is sent, so that an idle
/// rule on the server side cannot close the connection.
const PING_EVERY: Duration = Duration::from_secs(30);

/// The most levels per side that a top-of-book snapshot carries.
const TOP_DEPTH: usize = 5;

/// The `l2` price groupings, as optional significant figures and an optional
/// mantissa: the six the web app offers for BTC ($1, $2, $5, $10, $100 and
/// $1,000 at current prices, finest first) and, last, the one without
/// rounding. Connection `i` carries grouping `i`.
const GROUPINGS: [(Option<u8>, Option<u8>); 7] = [
    (Some(5), None),
    (Some(5), Some(2)),
    (Some(5), Some(5)),
    (Some(4), None),
    (Some(3), None),
    (Some(2), None),
    (None, None),
];

/// How many connections are open, one per `l2` price grouping.
pub const CONNECTIONS: usize = GROUPINGS.len();

/// The `l2` subscription for one grouping, as sent and as written into its
/// lines.
fn l2_subscription(grouping: usize) -> Value {
    let (figures, mantissa) = GROUPINGS[grouping];
    let mut subscription = json!({"type": "l2", "c": "BTC"});
    if let Some(figures) = figures {
        subscription["s"] = json!(figures);
    }
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
    l2: [Instant; CONNECTIONS],
}

impl Deadlines {
    fn new(now: Instant) -> Self {
        Self {
            full_book: now + FULL_BOOK_LIMIT,
            l2: [now + L2_LIMIT; CONNECTIONS],
        }
    }

    /// Moves the deadlines that a message of this kind on this connection
    /// meets.
    fn note(&mut self, connection: usize, received: &Received, now: Instant) {
        if connection == 0 && *received == Received::FullBook {
            self.full_book = now + FULL_BOOK_LIMIT;
        }
        if *received == Received::L2 {
            self.l2[connection] = now + L2_LIMIT;
        }
    }

    /// The earliest deadline.
    fn next(&self) -> Instant {
        self.l2.iter().copied().fold(self.full_book, Instant::min)
    }

    /// What has gone silent for too long at `now`, if anything, and the limit
    /// it exceeded.
    fn missed(&self, now: Instant) -> Option<(String, Duration)> {
        if now >= self.full_book {
            return Some(("no full book snapshot".to_owned(), FULL_BOOK_LIMIT));
        }
        let grouping = self.l2.iter().position(|deadline| now >= *deadline)?;
        let what = format!("no l2 message for {}", l2_subscription(grouping));
        Some((what, L2_LIMIT))
    }
}

/// Adds the connection's `l2` subscription to an `l2` message, the only place
/// that says which grouping the message is for. A message that already has
/// the field is an error, since writing over it would lose source data.
fn tag(value: &mut Value, connection: usize) -> Result<(), Error> {
    if value["channel"] != "l2" {
        return Ok(());
    }
    if let Some(map) = value.as_object_mut() {
        if map.contains_key("subscription") {
            return Err(Error::Tagged { connection });
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

/// Why the connections ended.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A connection could not be opened.
    #[error("cannot connect to {url}: {source}")]
    Connect {
        /// The address connected to.
        url: String,
        /// Why it failed.
        source: tokio_tungstenite::tungstenite::Error,
    },
    /// A subscription or ping could not be sent.
    #[error("cannot send on connection {connection} after {lines} lines: {source}")]
    Send {
        /// The connection's index.
        connection: usize,
        /// Messages handed out so far.
        lines: u64,
        /// Why it failed.
        source: tokio_tungstenite::tungstenite::Error,
    },
    /// A text message is not valid JSON.
    #[error("message after {lines} lines is not valid JSON: {source}")]
    Json {
        /// Messages handed out so far.
        lines: u64,
        /// What the parser rejected.
        source: serde_json::Error,
    },
    /// An `l2` message already has a `subscription` field.
    #[error("an l2 message on connection {connection} already has a subscription field")]
    Tagged {
        /// The connection's index.
        connection: usize,
    },
    /// A connection failed while reading.
    #[error("connection {connection} lost after {lines} lines: {source}")]
    Lost {
        /// The connection's index.
        connection: usize,
        /// Messages handed out so far.
        lines: u64,
        /// Why it failed.
        source: tokio_tungstenite::tungstenite::Error,
    },
    /// A connection was closed.
    #[error("connection {connection} closed after {lines} lines")]
    Closed {
        /// The connection's index.
        connection: usize,
        /// Messages handed out so far.
        lines: u64,
    },
    /// A feed stayed silent for longer than the limit.
    #[error("{what} for {limit:?} after {lines} lines")]
    Silent {
        /// What went silent.
        what: String,
        /// How long a feed may be silent.
        limit: Duration,
        /// Messages handed out so far.
        lines: u64,
    },
}

/// A message of one connection, or its end: `None` when the stream is over.
type Event = (
    usize,
    Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
);

/// The seven connections, subscribed, and the state that watches them.
pub struct Connections {
    sinks: Vec<SplitSink<Socket, Message>>,
    received: stream::SelectAll<BoxStream<'static, Event>>,
    deadlines: Deadlines,
    ping: Interval,
    lines: u64,
}

impl Connections {
    /// Opens one connection per `l2` price grouping to `url` and subscribes
    /// each to its feeds. The silence limits (30 s without a full book, 5 s
    /// without an `l2` message) start now.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Connect`] when a connection cannot be opened and
    /// [`Error::Send`] when a subscription cannot be sent.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let mut sinks = Vec::with_capacity(CONNECTIONS);
        let mut streams = Vec::with_capacity(CONNECTIONS);
        for index in 0..CONNECTIONS {
            let (socket, _) = connect_async(url).await.map_err(|source| Error::Connect {
                url: url.to_owned(),
                source,
            })?;
            let (mut sink, stream) = socket.split();
            for subscription in subscriptions(index) {
                let request = json!({"method": "subscribe", "subscription": subscription});
                sink.send(Message::text(request.to_string()))
                    .await
                    .map_err(|source| Error::Send {
                        connection: index,
                        lines: 0,
                        source,
                    })?;
            }
            sinks.push(sink);
            // Each stream ends with `None`, so a closed connection is seen by
            // index rather than just leaving the merged stream.
            streams.push(
                stream
                    .map(move |message| (index, Some(message)))
                    .chain(stream::once(async move { (index, None) }))
                    .boxed(),
            );
        }
        Ok(Self {
            sinks,
            received: select_all(streams),
            deadlines: Deadlines::new(Instant::now()),
            ping: interval_at(Instant::now() + PING_EVERY, PING_EVERY),
            lines: 0,
        })
    }

    /// Waits for the next message of any connection and returns it as JSON:
    /// `users` fields removed, `l2` messages tagged with their subscription.
    /// Sends the keep-alive pings on the way. Cancelling the wait loses
    /// nothing, except possibly a ping half sent.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when a connection is closed or lost, a message is
    /// not valid JSON or already has a `subscription`, or a feed stays
    /// silent; the connections are not usable afterwards.
    pub async fn next(&mut self) -> Result<Value, Error> {
        loop {
            let lines = self.lines;
            // Biased, messages first: a deadline is stamped when a message is
            // read here, so a caller that was away for longer than a limit
            // (engine backpressure, a slow disk, a sleeping laptop) finds
            // expired deadlines next to messages that arrived in time.
            // Picking at random would report a healthy connection silent.
            tokio::select! {
                biased;
                Some((connection, message)) = self.received.next() => match message {
                    Some(Ok(Message::Text(text))) => {
                        let mut value = serde_json::from_str::<Value>(&text)
                            .map_err(|source| Error::Json { lines, source })?;
                        self.deadlines.note(connection, &classify(&value), Instant::now());
                        strip_users(&mut value);
                        tag(&mut value, connection)?;
                        if value["channel"] == "error" {
                            warn!(%value, connection, "server reported an error");
                        }
                        self.lines += 1;
                        return Ok(value);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(source)) => {
                        return Err(Error::Lost { connection, lines, source });
                    }
                    None => return Err(Error::Closed { connection, lines }),
                },
                _ = self.ping.tick() => {
                    for (connection, sink) in self.sinks.iter_mut().enumerate() {
                        let ping = Message::text(json!({"method": "ping"}).to_string());
                        sink.send(ping).await.map_err(|source| Error::Send {
                            connection,
                            lines,
                            source,
                        })?;
                    }
                }
                () = sleep_until(self.deadlines.next()) => {
                    if let Some((what, limit)) = self.deadlines.missed(Instant::now()) {
                        return Err(Error::Silent { what, limit, lines });
                    }
                }
            }
        }
    }
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
        let rest: Vec<Vec<Value>> = (1..CONNECTIONS).map(subscriptions).collect();
        assert_eq!(
            rest,
            vec![
                vec![json!({"type": "l2", "c": "BTC", "s": 5, "m": 2})],
                vec![json!({"type": "l2", "c": "BTC", "s": 5, "m": 5})],
                vec![json!({"type": "l2", "c": "BTC", "s": 4})],
                vec![json!({"type": "l2", "c": "BTC", "s": 3})],
                vec![json!({"type": "l2", "c": "BTC", "s": 2})],
                vec![json!({"type": "l2", "c": "BTC"})],
            ]
        );
    }

    #[test]
    fn the_last_connection_subscribes_without_rounding_and_is_tagged_without_s() {
        let last = CONNECTIONS - 1;
        assert_eq!(subscriptions(last), vec![json!({"type": "l2", "c": "BTC"})]);
        let mut change = json!({"channel": "l2", "data": {"c": "abc"}});
        tag(&mut change, last).expect("tags");
        assert_eq!(
            change["subscription"],
            json!({"type": "l2", "c": "BTC"}),
            "no s key, not a null one"
        );
        // Its silence is named like any other connection's.
        let start = Instant::now();
        let mut deadlines = Deadlines::new(start);
        deadlines.note(0, &Received::FullBook, start + L2_LIMIT / 2);
        for connection in 0..last {
            deadlines.note(connection, &Received::L2, start + L2_LIMIT / 2);
        }
        assert_eq!(
            deadlines.missed(start + L2_LIMIT),
            Some((
                r#"no l2 message for {"c":"BTC","type":"l2"}"#.to_owned(),
                L2_LIMIT
            ))
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

    /// A local server that accepts one connection per grouping and closes the
    /// `closed`th right away, keeping the others open and silent. Returns its
    /// address.
    async fn local_server(closed: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a local port");
        let addr = listener.local_addr().expect("has an address");
        tokio::spawn(async move {
            let mut open = Vec::new();
            for index in 0..CONNECTIONS {
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
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn a_closed_connection_ends_the_run_and_is_named() {
        let url = local_server(3).await;
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let mut connections = Connections::connect(&url).await.expect("connects");
            connections.next().await
        })
        .await
        .expect("ends well before the idle limit");
        let message = result
            .expect_err("a closed connection is an error")
            .to_string();
        assert!(
            message.starts_with("connection 3 closed") || message.starts_with("connection 3 lost"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn messages_buffered_while_the_caller_was_away_are_not_reported_silent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a local port");
        let addr = listener.local_addr().expect("has an address");
        tokio::spawn(async move {
            let mut open = Vec::new();
            for _ in 0..CONNECTIONS {
                let (tcp, _) = listener.accept().await.expect("accepts");
                let mut socket = tokio_tungstenite::accept_async(tcp)
                    .await
                    .expect("handshakes");
                let l2 = json!({"channel": "l2", "data": {}}).to_string();
                socket.send(Message::text(l2)).await.expect("sends");
                open.push(socket);
            }
            std::future::pending::<()>().await;
        });
        let result = tokio::time::timeout(L2_LIMIT * 3, async {
            let mut connections = Connections::connect(&format!("ws://{addr}"))
                .await
                .expect("connects");
            // Longer than every l2 limit, with one message per connection
            // waiting: each read must return it instead of the expired limit.
            tokio::time::sleep(L2_LIMIT + Duration::from_millis(500)).await;
            for _ in 0..CONNECTIONS {
                connections.next().await?;
            }
            Ok::<(), Error>(())
        })
        .await
        .expect("the reads do not hang");
        result.expect("buffered messages are handed out, not reported silent");
    }

    #[test]
    fn each_connection_has_its_own_l2_deadline() {
        let start = Instant::now();
        let mut deadlines = Deadlines::new(start);
        let later = start + L2_LIMIT / 2;
        // Every connection but number 2 sends l2, and the main one sends a
        // full book; number 2 still runs out at the first limit.
        for connection in [0, 1, 3, 4, 5, 6] {
            deadlines.note(connection, &Received::L2, later);
        }
        deadlines.note(0, &Received::FullBook, later);
        assert_eq!(deadlines.next(), start + L2_LIMIT);
        assert_eq!(
            deadlines.missed(start + L2_LIMIT - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            deadlines.missed(start + L2_LIMIT),
            Some((
                r#"no l2 message for {"c":"BTC","m":5,"s":5,"type":"l2"}"#.to_owned(),
                L2_LIMIT
            ))
        );
        deadlines.note(2, &Received::L2, later);
        assert_eq!(deadlines.missed(start + L2_LIMIT), None);
        assert_eq!(deadlines.next(), later + L2_LIMIT);
    }

    #[test]
    fn only_a_full_book_on_the_main_connection_keeps_its_deadline_alive() {
        let start = Instant::now();
        let mut deadlines = Deadlines::new(start);
        let later = start + FULL_BOOK_LIMIT / 2;
        // Just inside the full book's limit, so that the l2 deadlines outlast it.
        let l2_at = start + FULL_BOOK_LIMIT - Duration::from_millis(1);
        for connection in 0..CONNECTIONS {
            deadlines.note(connection, &Received::L2, l2_at);
        }
        for connection in 1..CONNECTIONS {
            deadlines.note(connection, &Received::FullBook, later);
        }
        deadlines.note(0, &Received::Other, later);
        // The full book is now the only deadline left at the first limit, so
        // the loop must wake for it even while every l2 feed is flowing.
        assert_eq!(deadlines.next(), start + FULL_BOOK_LIMIT);
        assert_eq!(
            deadlines.missed(start + FULL_BOOK_LIMIT),
            Some(("no full book snapshot".to_owned(), FULL_BOOK_LIMIT))
        );
        deadlines.note(0, &Received::FullBook, later);
        assert_eq!(deadlines.missed(start + FULL_BOOK_LIMIT), None);
    }

    #[test]
    fn the_l2_deadline_is_missed_long_before_the_full_book_one() {
        let start = Instant::now();
        let deadlines = Deadlines::new(start);
        assert_eq!(deadlines.next(), start + L2_LIMIT);
        let (what, limit) = deadlines.missed(start + L2_LIMIT).expect("an l2 deadline");
        assert!(what.starts_with("no l2 message"), "{what}");
        assert_eq!(limit, L2_LIMIT);
        // Keeping every l2 feed alive leaves the full book's own, longer one.
        let mut deadlines = deadlines;
        for connection in 0..CONNECTIONS {
            deadlines.note(
                connection,
                &Received::L2,
                start + L2_LIMIT - Duration::from_millis(1),
            );
        }
        assert_eq!(deadlines.missed(start + L2_LIMIT), None);
        assert_eq!(
            deadlines.missed(start + FULL_BOOK_LIMIT),
            Some(("no full book snapshot".to_owned(), FULL_BOOK_LIMIT))
        );
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
