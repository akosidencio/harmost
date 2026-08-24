//! A slow origin that reports its own peak concurrency.
//!
//! Usage: slow-origin [PORT] [RENDER_MS]
//!
//! Every response carries `X-Origin-Concurrency` (in flight when the request
//! began) and `X-Origin-Peak` (highest seen since start). Those two headers are
//! how the benchmark proves the ceiling held without trusting the proxy's own
//! metrics.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Stats {
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3000);
    let render_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);

    let stats = Arc::new(Stats {
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        total: AtomicUsize::new(0),
    });

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("slow-origin on 127.0.0.1:{port}, {render_ms}ms per render");

    loop {
        let (mut sock, _) = listener.accept().await?;
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            // One request per connection keeps the fixture honest and simple.
            let Ok(n) = sock.read(&mut buf).await else { return };
            if n == 0 {
                return;
            }
            let head = String::from_utf8_lossy(&buf[..n]);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();

            let now = stats.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            stats.peak.fetch_max(now, Ordering::SeqCst);
            stats.total.fetch_add(1, Ordering::SeqCst);

            if path == "/healthz" {
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await;
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                return;
            }

            tokio::time::sleep(Duration::from_millis(render_ms)).await;

            let body = format!("<html><body>rendered {path}</body></html>");
            let peak = stats.peak.load(Ordering::SeqCst);
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html\r\n\
                 Content-Length: {}\r\n\
                 Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate\r\n\
                 X-Origin-Concurrency: {now}\r\n\
                 X-Origin-Peak: {peak}\r\n\
                 X-Origin-Total: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len(),
                stats.total.load(Ordering::SeqCst),
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            stats.in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
