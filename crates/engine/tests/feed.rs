//! End-to-end check of the WebSocket feed against a running service.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use engine::{RandomWalk, Step};
use futures_util::{SinkExt, StreamExt};
use protocol::{Command, FeedMessage, OrderType, Side, TimeInForce};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);
const PING: Duration = Duration::from_millis(100);

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(addr: SocketAddr) -> Client {
    let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect");
    client
}

async fn next_message(client: &mut Client) -> FeedMessage {
    loop {
        let frame = timeout(WAIT, client.next())
            .await
            .expect("feed message in time")
            .expect("connection open")
            .expect("valid frame");
        if let Message::Text(text) = frame {
            return serde_json::from_str(text.as_str()).expect("feed message");
        }
    }
}

/// Reads a snapshot, then `count` updates whose `seq` rises by one each time.
/// Returns the snapshot's `seq` and the last update's.
async fn follow(client: &mut Client, count: u64) -> (u64, u64) {
    let FeedMessage::Snapshot {
        seq: start,
        bids,
        asks,
        market,
        ..
    } = next_message(client).await
    else {
        panic!("first message must be a snapshot");
    };
    assert_eq!(market.symbol, "BTC-USD");
    assert!(bids.windows(2).all(|w| w[0].price > w[1].price));
    assert!(asks.windows(2).all(|w| w[0].price < w[1].price));
    for offset in 1..=count {
        let FeedMessage::Update { seq, .. } = next_message(client).await else {
            panic!("expected an update");
        };
        assert_eq!(seq, start + offset);
    }
    (start, start + count)
}

type Server = JoinHandle<io::Result<()>>;

async fn start(ping_interval: Duration) -> (SocketAddr, CancellationToken, Server) {
    start_capped(ping_interval, engine::MAX_CLIENTS).await
}

async fn start_capped(
    ping_interval: Duration,
    max_clients: usize,
) -> (SocketAddr, CancellationToken, Server) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(engine::run(
        listener,
        RandomWalk::new(7, 0),
        shutdown.clone(),
        ping_interval,
        max_clients,
    ));
    (addr, shutdown, server)
}

async fn stop(shutdown: CancellationToken, server: Server) {
    shutdown.cancel();
    timeout(WAIT, server)
        .await
        .expect("clean shutdown")
        .expect("server task")
        .expect("server result");
}

#[tokio::test]
async fn snapshot_then_consecutive_updates_for_early_and_late_clients() {
    let (addr, shutdown, server) = start(engine::PING_INTERVAL).await;

    let mut first = connect(addr).await;
    let (_, seen) = follow(&mut first, 4).await;

    let mut late = connect(addr).await;
    let (start, _) = follow(&mut late, 2).await;
    assert!(
        start >= seen,
        "the late snapshot covers the updates already sent"
    );

    stop(shutdown, server).await;
}

#[tokio::test]
async fn a_silent_client_is_dropped() {
    let (addr, shutdown, server) = start(PING).await;
    let mut client = connect(addr).await;
    next_message(&mut client).await;

    // A tungstenite client answers pings only while it reads, so sleeping
    // keeps it silent.
    tokio::time::sleep(PING * 10).await;
    // Any end counts: a reset can discard the server's Close frame, so it
    // cannot be required.
    let closed = timeout(WAIT, async {
        while let Some(Ok(frame)) = client.next().await {
            if matches!(frame, Message::Close(_)) {
                return;
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the server must close the silent connection"
    );
    stop(shutdown, server).await;
}

#[tokio::test]
async fn a_responsive_client_stays_connected_and_is_pinged() {
    let (addr, shutdown, server) = start(PING).await;
    let mut client = connect(addr).await;
    next_message(&mut client).await;

    let mut pings = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Ok(frame) = tokio::time::timeout_at(deadline, client.next()).await {
        match frame.expect("connection open").expect("valid frame") {
            Message::Ping(_) => pings += 1,
            Message::Close(_) => panic!("a responsive client was dropped"),
            _ => {}
        }
    }
    assert!(pings > 0, "the server must ping");
    stop(shutdown, server).await;
}

#[tokio::test]
async fn a_zero_ping_interval_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let result = timeout(
        WAIT,
        engine::run(
            listener,
            RandomWalk::new(7, 0),
            CancellationToken::new(),
            Duration::ZERO,
            engine::MAX_CLIENTS,
        ),
    )
    .await
    .expect("run must return instead of serving");
    let err = result.expect_err("zero interval");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn a_client_cap_above_the_limit_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let result = timeout(
        WAIT,
        engine::run(
            listener,
            RandomWalk::new(7, 0),
            CancellationToken::new(),
            PING,
            usize::MAX,
        ),
    )
    .await
    .expect("run must return instead of serving");
    assert_eq!(
        result.expect_err("huge cap").kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn a_zero_client_cap_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let result = timeout(
        WAIT,
        engine::run(
            listener,
            RandomWalk::new(7, 0),
            CancellationToken::new(),
            PING,
            0,
        ),
    )
    .await
    .expect("run must return instead of serving");
    assert_eq!(
        result.expect_err("zero cap").kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn shutdown_stops_an_idle_flow() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let cancel = |order_id, wait| Step {
        command: Command::Cancel { order_id },
        ts: order_id,
        wait,
    };
    let mut steps = vec![
        cancel(1, Duration::from_secs(3600)),
        cancel(2, Duration::ZERO),
    ]
    .into_iter();
    // The source pulls the second step right before it pauses for the first
    // step's hour-long wait, so this signal says it is about to idle.
    let (pulled, second_pulled) = std::sync::mpsc::channel();
    let mut count = 0;
    let flow = std::iter::from_fn(move || {
        count += 1;
        if count == 2 {
            let _ = pulled.send(());
        }
        steps.next()
    });
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(engine::run(
        listener,
        flow,
        shutdown.clone(),
        engine::PING_INTERVAL,
        engine::MAX_CLIENTS,
    ));
    tokio::task::spawn_blocking(move || second_pulled.recv_timeout(WAIT))
        .await
        .expect("wait task")
        .expect("the source reaches the second step");
    stop(shutdown, server).await;
}

/// The last command's level must reach clients that stay connected until the
/// server closes them. The publisher cancels shutdown at the moment it
/// ends, so losing the final frame is a race; it repeats to make a regression
/// very likely to show.
#[tokio::test(flavor = "multi_thread")]
async fn the_final_update_reaches_clients_before_the_close() {
    let place = |order_id, price, wait| Step {
        command: Command::Place {
            order_id,
            side: Side::Buy,
            order_type: OrderType::Limit {
                price,
                time_in_force: TimeInForce::Gtc,
            },
            qty: 1,
        },
        ts: order_id,
        wait,
    };
    for _ in 0..10 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let flow = vec![
            place(1, 100, Duration::from_millis(250)),
            place(2, 90, Duration::ZERO),
        ];
        let server = tokio::spawn(engine::run(
            listener,
            flow.into_iter(),
            CancellationToken::new(),
            engine::PING_INTERVAL,
            engine::MAX_CLIENTS,
        ));
        let mut client = connect(addr).await;
        let mut bids = std::collections::BTreeSet::new();
        while let Ok(Some(Ok(frame))) = timeout(WAIT, client.next()).await {
            let Message::Text(text) = frame else { continue };
            match serde_json::from_str(text.as_str()).expect("feed message") {
                FeedMessage::Snapshot { bids: levels, .. } => {
                    bids = levels.iter().map(|l| l.price).collect();
                }
                FeedMessage::Update { bids: levels, .. } => {
                    for level in levels {
                        if level.qty == 0 {
                            bids.remove(&level.price);
                        } else {
                            bids.insert(level.price);
                        }
                    }
                }
                FeedMessage::Heartbeat { .. } => {}
            }
        }
        assert!(
            bids.contains(&90),
            "the last command's level is in the feed"
        );
        timeout(WAIT, server)
            .await
            .expect("server ends with its flow")
            .expect("server task")
            .expect("server result");
    }
}

/// Whether the server ends the connection within `WAIT`: a close frame, an
/// error or the end of the stream all count.
async fn is_closed(client: &mut Client) -> bool {
    timeout(WAIT, async {
        while let Some(Ok(frame)) = client.next().await {
            if matches!(frame, Message::Close(_)) {
                return;
            }
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn clients_beyond_the_cap_get_503_until_a_slot_frees() {
    let (addr, shutdown, server) = start_capped(engine::PING_INTERVAL, 2).await;
    let mut first = connect(addr).await;
    let mut second = connect(addr).await;
    follow(&mut first, 1).await;
    follow(&mut second, 1).await;

    let refused = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await;
    let Err(tokio_tungstenite::tungstenite::Error::Http(response)) = refused else {
        panic!("the third client must be refused with an HTTP response");
    };
    assert_eq!(response.status(), 503);

    drop(first);
    let mut third = timeout(WAIT, async {
        loop {
            // The server notices the closed connection asynchronously.
            if let Ok((client, _)) =
                tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await
            {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a slot must free up after a client leaves");
    follow(&mut third, 1).await;
    next_message(&mut second).await;
    stop(shutdown, server).await;
}

#[tokio::test]
async fn a_client_sending_data_or_an_oversized_frame_is_dropped() {
    let (addr, shutdown, server) = start(engine::PING_INTERVAL).await;
    let mut bystander = connect(addr).await;
    for message in [
        Message::Text("hello".into()),
        Message::Binary(vec![0_u8; 64 * 1024].into()),
    ] {
        let mut client = connect(addr).await;
        next_message(&mut client).await;
        // The server may reset the connection mid-send for the big frame.
        let _ = client.send(message).await;
        assert!(is_closed(&mut client).await, "the server must drop it");
    }
    follow(&mut bystander, 3).await;
    stop(shutdown, server).await;
}

/// Completes the handshake by hand, then sends only the header of a masked
/// text frame that announces 1 MiB. A server without a frame size cap waits
/// for the payload that never comes; one with a cap rejects the frame on the
/// header alone. This isolates the cap from the "data is dropped" rule, which
/// only applies once a whole message has arrived.
#[tokio::test]
async fn a_frame_header_announcing_too_much_is_rejected_before_its_payload() {
    use std::io::{Read, Write};

    let (addr, shutdown, server) = start(engine::PING_INTERVAL).await;
    let dropped = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        stream.set_write_timeout(Some(WAIT)).expect("write timeout");
        let request = format!(
            "GET /ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).expect("handshake");
        let mut seen = Vec::new();
        let mut chunk = [0_u8; 4096];
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).expect("handshake response");
            assert!(n > 0, "closed during the handshake");
            seen.extend_from_slice(&chunk[..n]);
        }
        assert!(seen.starts_with(b"HTTP/1.1 101"), "upgrade must succeed");

        let mut header = vec![0x81, 0x80 | 127];
        header.extend_from_slice(&(1_u64 << 20).to_be_bytes());
        header.extend_from_slice(&[0, 0, 0, 0]);
        stream.write_all(&header).expect("frame header");
        // The feed keeps sending updates, so reads never time out; the
        // deadline is what tells a prompt rejection from a server that waits
        // for the payload until its liveness check gives up (15 s here).
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match stream.read(&mut chunk) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::ConnectionReset => return true,
                Err(_) => return false,
            }
        }
        false
    })
    .await
    .expect("client thread");
    assert!(dropped, "the server must reject the frame on its header");
    stop(shutdown, server).await;
}
