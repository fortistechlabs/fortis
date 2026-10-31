//! Task 19: a request whose body the handler never reads must not leave that
//! body on the connection to be parsed as the NEXT request (request smuggling
//! behind a proxy that reuses upstream connections). Real sockets, the patched
//! vendored `tiny_http`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A server whose handler answers at once and NEVER reads a body (like the
/// edge's 401/404/413 exits), recording the paths it was asked for.
fn server() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    thread::spawn(move || {
        for req in server.incoming_requests() {
            s2.lock().unwrap().push(req.url().to_string());
            let _ = req.respond(tiny_http::Response::from_string("ok"));
        }
    });
    (addr, seen)
}

/// Sends `bytes`, then reads until the server closes or goes quiet.
fn talk(addr: SocketAddr, bytes: &[u8]) -> (String, bool) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(1500))).unwrap();
    s.write_all(bytes).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let closed = loop {
        match s.read(&mut buf) {
            Ok(0) => break true,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break false, // timed out: the connection is still open
        }
    };
    (String::from_utf8_lossy(&out).into_owned(), closed)
}

const SMUGGLED: &str = "GET /smuggled HTTP/1.1\r\nHost: t\r\n\r\n";

#[test]
fn an_unread_large_content_length_body_is_not_parsed_as_a_request() {
    let (addr, seen) = server();
    let mut body = SMUGGLED.as_bytes().to_vec();
    body.resize(100_000, b'a'); // over the 64 KiB the server will drain
    let mut req = b"POST /x HTTP/1.1\r\nHost: t\r\nContent-Length: 100000\r\n\r\n".to_vec();
    req.extend_from_slice(&body);
    let (out, closed) = talk(addr, &req);
    assert_eq!(out.matches("HTTP/1.1 200").count(), 1, "exactly one response: {out:?}");
    assert!(closed, "the connection must be closed after the response");
    assert_eq!(*seen.lock().unwrap(), vec!["/x".to_string()], "the body was parsed as a request");
}

#[test]
fn an_unread_chunked_body_is_not_parsed_as_a_request() {
    // The server never reads the body, so it never checks that it really is
    // chunked: the "body" can be a plain request.
    let (addr, seen) = server();
    let req = format!("POST /x HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n{SMUGGLED}");
    let (out, closed) = talk(addr, req.as_bytes());
    assert_eq!(out.matches("HTTP/1.1 200").count(), 1, "exactly one response: {out:?}");
    assert!(closed);
    assert_eq!(*seen.lock().unwrap(), vec!["/x".to_string()]);
}

#[test]
fn small_unread_bodies_still_keep_the_connection_usable() {
    let (addr, seen) = server();
    // 5 000 bytes (drained by the server) then a normal second request.
    let body = vec![b'b'; 5_000];
    let mut req = b"POST /x HTTP/1.1\r\nHost: t\r\nContent-Length: 5000\r\n\r\n".to_vec();
    req.extend_from_slice(&body);
    req.extend_from_slice(b"GET /y HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    let (out, _) = talk(addr, &req);
    assert_eq!(out.matches("HTTP/1.1 200").count(), 2, "both answered: {out:?}");
    assert_eq!(*seen.lock().unwrap(), vec!["/x".to_string(), "/y".to_string()]);
}

#[test]
fn a_huge_declared_length_neither_allocates_it_nor_keeps_the_connection() {
    // The original bug: the server tried to allocate the declared length.
    let (addr, seen) = server();
    let req = b"POST /x HTTP/1.1\r\nHost: t\r\nContent-Length: 4000000000000\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n";
    let (out, closed) = talk(addr, req);
    assert_eq!(out.matches("HTTP/1.1 200").count(), 1, "{out:?}");
    assert!(closed);
    assert_eq!(*seen.lock().unwrap(), vec!["/x".to_string()]);
}
