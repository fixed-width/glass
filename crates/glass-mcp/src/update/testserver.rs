//! A four-route HTTP server for the update tests, hand-rolled over `std::net::TcpListener`.
//!
//! Deliberately not axum: that would tie these tests to the optional `network` feature, and glass
//! already hand-rolls loopback HTTP in `setup::fetch_health`. Four routes, one thread, no
//! dependencies — and because the base URL is a `ReleaseSource` constructor argument, the tests
//! drive the real code path rather than a mock of it.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

pub(crate) struct FakeRelease {
    port: u16,
    _handle: thread::JoinHandle<()>,
}

struct Routes {
    tag: String,
    asset_name: String,
    asset: Vec<u8>,
    sidecar: String,
}

impl FakeRelease {
    /// Serve `/fixed-width/glass/releases/latest` as a 302 to `tag`, plus the named asset and its
    /// `.sha256` sidecar under `/releases/download/<tag>/`. Everything else 404s.
    pub(crate) fn start(tag: &str, asset_name: &str, asset: &[u8], sidecar: &str) -> FakeRelease {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let routes = Arc::new(Routes {
            tag: tag.to_string(),
            asset_name: asset_name.to_string(),
            asset: asset.to_vec(),
            sidecar: sidecar.to_string(),
        });
        let handle = thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = Arc::clone(&routes);
                // Serve sequentially: the update flow makes its requests one at a time, and a
                // single-threaded server makes a hung request obvious rather than hidden.
                serve_one(stream, &routes, port);
            }
        });
        FakeRelease {
            port,
            _handle: handle,
        }
    }

    pub(crate) fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn serve_one(mut stream: TcpStream, routes: &Routes, port: u16) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string();
    // Drain the headers so the client sees a clean response rather than a reset.
    for line in reader.lines() {
        match line {
            Ok(l) if l.is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let latest = "/fixed-width/glass/releases/latest";
    let download = format!(
        "/fixed-width/glass/releases/download/{}/{}",
        routes.tag, routes.asset_name
    );
    let sidecar = format!("{download}.sha256");

    // Two routes that exist only to drive `following_client`'s security policy: one redirect that
    // changes the scheme, and one that never terminates. Without them the hop cap and the
    // scheme-downgrade refusal have no committed coverage at all.
    if path == "/redirect/scheme" {
        let _ = write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: https://127.0.0.1:{port}/fixed-width/glass/releases/latest\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.flush();
        return;
    }
    if let Some(n) = path.strip_prefix("/redirect/chain/") {
        let next = n.parse::<u32>().unwrap_or(0) + 1;
        let _ = write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/redirect/chain/{next}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.flush();
        return;
    }
    if path == latest {
        let location = format!(
            "http://127.0.0.1:{port}/fixed-width/glass/releases/tag/{}",
            routes.tag
        );
        let _ = write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    } else if path == sidecar {
        let body = routes.sidecar.as_bytes();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(body);
    } else if path == download {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            routes.asset.len()
        );
        let _ = stream.write_all(&routes.asset);
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }
    let _ = stream.flush();
}
