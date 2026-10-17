//! End-to-end check of the WebSocket feed against a running service.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use protocol::FeedMessage;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);

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

#[tokio::test]
async fn snapshot_then_consecutive_updates_for_early_and_late_clients() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(engine::run(listener, Some(7), shutdown.clone()));

    let mut first = connect(addr).await;
    let (_, seen) = follow(&mut first, 4).await;

    let mut late = connect(addr).await;
    let (start, _) = follow(&mut late, 2).await;
    assert!(
        start >= seen,
        "the late snapshot covers the updates already sent"
    );

    shutdown.cancel();
    timeout(WAIT, server)
        .await
        .expect("clean shutdown")
        .expect("server task")
        .expect("server result");
}
