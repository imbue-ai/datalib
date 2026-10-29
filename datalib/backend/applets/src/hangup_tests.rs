//! An applet drops a request's work when its caller hangs up. The
//! gateway closes its connection to the applet when the browser gives a
//! search up (`datalib-http`'s `applets::Hangup`); without this, every
//! search a person typed past would still be ranked to the end.

use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

/// Says so when the handler holding it is dropped.
struct OnDrop(Option<oneshot::Sender<()>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

/// A handler waiting on work that never finishes is dropped once the
/// connection it answers on closes, the way every applet is served.
#[tokio::test]
async fn a_request_whose_caller_hangs_up_is_dropped() {
    let (started_tx, mut started) = mpsc::unbounded_channel::<oneshot::Receiver<()>>();
    let app = Router::new().route(
        "/search",
        get(move || {
            let started_tx = started_tx.clone();
            async move {
                let (dropped_tx, dropped) = oneshot::channel();
                let _guard = OnDrop(Some(dropped_tx));
                let _ = started_tx.send(dropped);
                std::future::pending::<()>().await;
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let mut caller = tokio::net::TcpStream::connect(addr).await.unwrap();
    caller
        .write_all(b"GET /search HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let dropped = tokio::time::timeout(Duration::from_secs(10), started.recv())
        .await
        .expect("the handler never started")
        .unwrap();

    drop(caller);
    tokio::time::timeout(Duration::from_secs(10), dropped)
        .await
        .expect("the handler outlived its caller")
        .unwrap();
}
