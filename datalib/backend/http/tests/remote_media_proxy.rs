//! A remote media fetch goes to the address it judged, never through a
//! proxy the environment names: the proxy would resolve the host again
//! itself, and a name that answered with a public address to us could
//! answer the proxy with a private one. Its own binary, because the
//! proxy variables it sets would reach every test sharing the process.

use datalib_http::remote_media::{fetch, RemotePolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A server that answers every request with `body` as a PNG.
async fn serve(body: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(head.as_bytes()).await;
            let _ = conn.write_all(body).await;
        }
    });
    port
}

#[tokio::test]
async fn a_proxy_in_the_environment_is_not_used() {
    let target = serve(b"from the target").await;
    let proxy = serve(b"from the proxy").await;
    let proxy_url = format!("http://127.0.0.1:{proxy}");
    for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        std::env::set_var(var, &proxy_url);
    }
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");

    let got = fetch(
        &format!("http://127.0.0.1:{target}/a.png"),
        &RemotePolicy {
            allow_private: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(got.body, b"from the target");
}
