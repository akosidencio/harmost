//! A slow origin that reports its own peak concurrency.
//!
//! Usage: slow-origin [PORT] [RENDER_MS]
//!
//! Every response carries `X-Origin-Concurrency` (in flight when the request
//! began) and `X-Origin-Peak` (highest seen since start). Those two headers are
//! how the benchmark proves the ceiling held without trusting the proxy's own
//! metrics.
//!
//! Three control endpoints exist so a benchmark never has to infer origin work
//! from the proxy's own logs or from headers that may have been replayed out of
//! the proxy's cache:
//!
//! * `GET /__stats`  — `{"in_flight":n,"peak":n,"total":n}` counted by the
//!   origin itself. This is the ground truth for "how many renders did this
//!   actually cost".
//! * `GET /__reset`  — zero the counters, so one process can measure several
//!   phases without a restart clouding the numbers.
//! * `GET /healthz`  — liveness, deliberately excluded from the counters so an
//!   active health check cannot inflate a render count.

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
            let Ok(n) = sock.read(&mut buf).await else {
                return;
            };
            if n == 0 {
                return;
            }
            let head = String::from_utf8_lossy(&buf[..n]);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();

            // Control endpoints are answered before the counters are touched.
            // A health probe or a stats poll is not origin work, and counting
            // it would corrupt the very number the benchmarks assert on.
            if let Some(body) = control_response(&path, &stats) {
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                return;
            }

            let now = stats.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            stats.peak.fetch_max(now, Ordering::SeqCst);
            let seq = stats.total.fetch_add(1, Ordering::SeqCst) + 1;

            // /stream/<n> emits n chunks with a render delay between each,
            // the way a server-rendered page streams a shell and then fills
            // in suspended regions. A buffered response cannot exercise
            // whether coalesced waiters receive chunks as they are produced.
            if path.starts_with("/stream") || path.starts_with("/bigstream") {
                let chunks: usize = path
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(4);
                let head = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/html\r\n\
                     Transfer-Encoding: chunked\r\n\
                     Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate\r\n\
                     X-Origin-Total: {seq}\r\n\
                     Connection: close\r\n\r\n"
                );
                if sock.write_all(head.as_bytes()).await.is_err() {
                    stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                let _ = sock.flush().await;
                for i in 0..chunks {
                    // The shell goes out immediately; everything after it
                    // costs a render delay.
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(render_ms)).await;
                    }
                    // /bigstream emits chunks large enough that a rate-limited
                    // reader actually stalls the write; /stream keeps them tiny
                    // so the coalescing test stays fast.
                    let piece = if path.starts_with("/bigstream") {
                        format!("<div>{}</div>", "x".repeat(256 * 1024))
                    } else {
                        format!("<div>chunk {i}</div>")
                    };
                    let framed = format!("{:x}\r\n{piece}\r\n", piece.len());
                    if sock.write_all(framed.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = sock.flush().await;
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
                let _ = sock.flush().await;
                stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                return;
            }

            tokio::time::sleep(Duration::from_millis(render_ms)).await;

            // /private/* sets a session cookie. Nothing that does this may
            // ever be shared between requests, no matter what the route
            // configuration claims.
            let set_cookie = if path.starts_with("/private") {
                format!("Set-Cookie: session=user-{seq}; Path=/\r\n")
            } else {
                String::new()
            };
            // /big/* returns a realistically sized SSR document so a slow
            // client has something to be slow about.
            // /big/<MiB> returns a body of that size, so a test can pick one
            // large enough to exceed the socket buffers between origin and
            // client and actually block the downstream write.
            let body = if path.starts_with("/big") {
                let mib: usize = path
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                let filler = "x".repeat(mib * 1024 * 1024);
                format!("<html><body>rendered {path}{filler}</body></html>")
            } else {
                format!("<html><body>rendered {path}</body></html>")
            };
            let peak = stats.peak.load(Ordering::SeqCst);
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html\r\n\
                 Content-Length: {}\r\n\
                 Cache-Control: private, no-cache, no-store, max-age=0, must-revalidate\r\n\
                 X-Origin-Concurrency: {now}\r\n\
                 X-Origin-Peak: {peak}\r\n\
                 X-Origin-Total: {}\r\n\
                 {set_cookie}\
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

/// Answer the non-render endpoints, or `None` if this path is a real render.
fn control_response(path: &str, stats: &Stats) -> Option<String> {
    let body = match path {
        "/healthz" => "ok".to_string(),
        "/__stats" => format!(
            "{{\"in_flight\":{},\"peak\":{},\"total\":{}}}",
            stats.in_flight.load(Ordering::SeqCst),
            stats.peak.load(Ordering::SeqCst),
            stats.total.load(Ordering::SeqCst),
        ),
        "/__reset" => {
            // in_flight is deliberately not cleared: it is a live count, and
            // zeroing it while requests are in flight would make it drift
            // negative-by-saturation as they complete.
            stats
                .peak
                .store(stats.in_flight.load(Ordering::SeqCst), Ordering::SeqCst);
            stats.total.store(0, Ordering::SeqCst);
            "reset".to_string()
        }
        _ => return None,
    };
    Some(format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Cache-Control: no-store\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len(),
    ))
}
