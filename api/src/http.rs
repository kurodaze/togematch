//! Just enough HTTP/1.1 for the API: one GET per connection, its head read within a size and
//! time limit, since anyone who can reach the port can send one.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant, SystemTime};

/// A request head this big isn't one of ours.
const MAX_HEAD: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
/// How long a caller gets to send its request, and to take the answer.
const PATIENCE: Duration = Duration::from_secs(10);
/// How long a caller gets to close its end after the answer.
const LINGER: Duration = Duration::from_secs(2);

pub struct Request {
    pub method: String,
    /// path and query, as sent
    pub target: String,
}

/// Reads a request's head. `Err(Some(status))` for one to refuse, `Err(None)` when the caller
/// went quiet or away.
pub fn read(mut stream: &TcpStream) -> Result<Request, Option<u16>> {
    let deadline = Instant::now() + PATIENCE;
    let mut head = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return Err(None);
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(None),
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(&head) {
            Ok(httparse::Status::Complete(_)) => {
                return Ok(Request {
                    method: request.method.unwrap_or_default().to_string(),
                    target: request.path.unwrap_or_default().to_string(),
                });
            }
            Ok(httparse::Status::Partial) if head.len() < MAX_HEAD => {}
            Ok(httparse::Status::Partial) | Err(httparse::Error::TooManyHeaders) => {
                return Err(Some(431));
            }
            Err(_) => return Err(Some(400)),
        }
    }
}

/// Answers with a JSON body and closes the connection.
pub fn respond(stream: &TcpStream, status: u16, headers: &[(&str, &str)], body: &str) {
    write(stream, status, headers, body);
    linger(stream);
}

fn write(mut stream: &TcpStream, status: u16, headers: &[(&str, &str)], body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "",
    };
    let extra: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let answer = format!(
        "HTTP/1.1 {status} {reason}\r\nDate: {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        httpdate::fmt_http_date(SystemTime::now()),
        body.len()
    );
    let _ = stream.set_write_timeout(Some(PATIENCE));
    let _ = stream.write_all(answer.as_bytes());
}

/// Closes in stages (RFC 9112, 9.6): closing on a caller still sending, like one whose head was
/// too big to read, resets the connection, and the reset can wipe out the answer before it's read.
fn linger(mut stream: &TcpStream) {
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    let deadline = Instant::now() + LINGER;
    let mut rest = [0; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        if !matches!(stream.read(&mut rest), Ok(1..)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// What `read` makes of these bytes from a caller that then hangs up.
    fn send(bytes: &[u8]) -> Result<Request, Option<u16>> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bytes = bytes.to_vec();
        let caller = thread::spawn(move || {
            let _ = TcpStream::connect(addr).unwrap().write_all(&bytes);
        });
        let (stream, _) = listener.accept().unwrap();
        let read = read(&stream);
        caller.join().unwrap();
        read
    }

    #[test]
    fn reads_a_request_within_limits() {
        let request = send(b"GET /match?q=dota HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/match?q=dota");
        assert_eq!(send(b"not http\r\n\r\n").err(), Some(Some(400)));
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(send(huge.as_bytes()).err(), Some(Some(431)));
        assert_eq!(send(b"GET / HTTP/1.1\r\n").err(), Some(None));
    }

    #[test]
    fn a_caller_still_sending_gets_the_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let status = read(&stream).err().flatten().unwrap();
            respond(&stream, status, &[], "{}");
        });
        let mut caller = TcpStream::connect(addr).unwrap();
        let head = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(4 * MAX_HEAD));
        let _ = caller.write_all(head.as_bytes());
        let mut answer = String::new();
        caller.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 431 "), "{answer}");
        server.join().unwrap();
    }
}
