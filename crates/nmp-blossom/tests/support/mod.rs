//! Minimal scripted HTTP/1.1 test double shared by the integration
//! contract suites (`upload_contract.rs`, `mirror_delete_list_contract.rs`
//! -- #545/#551). CRITICAL (#538 lesson): the mock reads the FULL request
//! -- request line, headers, AND the Content-Length body -- before it
//! replies or closes, so the client never observes an early close (macOS
//! mio ECONNRESET flakes). Each server serves exactly ONE connection; one
//! server per exchange keeps every exchange on a fresh port, so reqwest
//! connection pooling can never route a second request past the accept
//! loop.

// Each integration test crate compiles this module independently and none
// uses every item, so per-crate "unused" lints are expected noise here.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

#[derive(Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// Header names lower-cased at record time.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub struct ScriptedResponse {
    pub status_line: &'static str,
    pub extra_headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

pub struct MockServer {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
    pub accepted: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl MockServer {
    /// Serve exactly ONE connection with `response`, recording the
    /// fully-read request. One connection per server keeps every test
    /// exchange on a fresh port, so reqwest connection pooling can
    /// never route a second request past the accept loop.
    pub fn serve_one(response: ScriptedResponse) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
        let port = listener.local_addr().expect("mock server addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let thread_requests = Arc::clone(&requests);
        let thread_accepted = Arc::clone(&accepted);
        let handle = std::thread::spawn(move || {
            let (mut stream, _peer) = listener.accept().expect("accept mock connection");
            thread_accepted.fetch_add(1, Ordering::SeqCst);
            let request = read_full_request(&mut stream);
            thread_requests.lock().expect("requests lock").push(request);
            let mut wire = format!("{}\r\n", response.status_line).into_bytes();
            for (name, value) in &response.extra_headers {
                wire.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
            }
            wire.extend_from_slice(
                format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    response.body.len()
                )
                .as_bytes(),
            );
            wire.extend_from_slice(&response.body);
            stream.write_all(&wire).expect("write mock response");
            stream.flush().expect("flush mock response");
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            accepted,
            handle,
        }
    }

    pub fn join(self) -> Vec<RecordedRequest> {
        self.handle.join().expect("mock server thread");
        Arc::try_unwrap(self.requests)
            .expect("all request handles released")
            .into_inner()
            .expect("requests lock")
    }
}

fn read_full_request(stream: &mut std::net::TcpStream) -> RecordedRequest {
    let mut received = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let count = stream.read(&mut buffer).expect("read request headers");
        assert!(count > 0, "HTTP request ended before its headers");
        received.extend_from_slice(&buffer[..count]);
    };
    let header_text = String::from_utf8_lossy(&received[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = received[header_end..].to_vec();
    while body.len() < content_length {
        let count = stream.read(&mut buffer).expect("read request body");
        assert!(count > 0, "HTTP request ended before its full body");
        body.extend_from_slice(&buffer[..count]);
    }
    body.truncate(content_length);
    RecordedRequest {
        method,
        path,
        headers,
        body,
    }
}
