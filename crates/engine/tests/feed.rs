//! End-to-end check of the WebSocket feed against a running service.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use engine::{RandomWalk, Step};
use futures_util::StreamExt;
use protocol::{Command, FeedMessage};
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
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(engine::run(
        listener,
        RandomWalk::new(7, 0),
        shutdown.clone(),
        ping_interval,
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
        ),
    )
    .await
    .expect("run must return instead of serving");
    let err = result.expect_err("zero interval");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
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
    ));
    tokio::task::spawn_blocking(move || second_pulled.recv_timeout(WAIT))
        .await
        .expect("wait task")
        .expect("the source reaches the second step");
    stop(shutdown, server).await;
}
