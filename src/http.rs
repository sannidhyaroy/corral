//! A minimal HTTP/1.1 client for Corral's own servers on this machine: plain TCP to 127.0.0.1, bodies plain or
//! chunked, read as they arrive so event streams work too.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

/// How long a short request may take; event streams pass `None` and wait as long as the server talks.
pub const SHORT: Option<Duration> = Some(Duration::from_secs(5));

/// Sends a request to `port` on this machine and hands each piece of the body to `on_body` as it arrives, until
/// the body ends or `on_body` returns false. Returns the status code.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Option<Duration>,
    on_body: impl FnMut(&[u8]) -> bool,
) -> io::Result<u16> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = match timeout {
        Some(timeout) => TcpStream::connect_timeout(&address, timeout)?,
        None => TcpStream::connect(address)?,
    };
    stream.set_read_timeout(timeout)?;
    let body_headers = body
        .map(|body| format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()))
        .unwrap_or_default();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{body_headers}\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.unwrap_or_default().as_bytes())?;

    let mut reader = BufReader::new(stream);
    let (status, chunked) = read_head(&mut reader)?;
    read_body(&mut reader, chunked, on_body)?;
    Ok(status)
}

/// A GET answered with 200 and JSON; `None` otherwise.
pub fn get_json(port: u16, path: &str) -> Option<Value> {
    let mut body = Vec::new();
    let status = request(port, "GET", path, None, SHORT, |piece| {
        body.extend_from_slice(piece);
        true
    })
    .ok()?;
    (status == 200).then(|| serde_json::from_slice(&body).ok()).flatten()
}

/// A POST with a JSON body; true if the server answered with success.
pub fn post_json(port: u16, path: &str, body: &Value) -> bool {
    request(port, "POST", path, Some(&body.to_string()), SHORT, |_| true)
        .is_ok_and(|status| (200..300).contains(&status))
}

/// The status code, and whether the body is chunked.
fn read_head(reader: &mut impl BufRead) -> io::Result<(u16, bool)> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an HTTP response"))?;
    let mut chunked = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let header = line.trim_end().to_ascii_lowercase();
        if header.is_empty() {
            break;
        }
        if header.starts_with("transfer-encoding:") && header.contains("chunked") {
            chunked = true;
        }
    }
    Ok((status, chunked))
}

fn read_body(reader: &mut impl BufRead, chunked: bool, mut on_body: impl FnMut(&[u8]) -> bool) -> io::Result<()> {
    if !chunked {
        let mut buffer = [0u8; 8192];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 || !on_body(&buffer[..read]) {
                return Ok(());
            }
        }
    }
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            return Ok(());
        }
        let size_hex = size_line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_hex, 16).unwrap_or(0);
        if size == 0 {
            return Ok(());
        }
        // The chunk and the line break after it
        let mut chunk = vec![0u8; size + 2];
        reader.read_exact(&mut chunk)?;
        if !on_body(&chunk[..size]) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_all(response: &str) -> (u16, String) {
        let mut reader = Cursor::new(response.as_bytes());
        let (status, chunked) = read_head(&mut reader).unwrap();
        let mut body = Vec::new();
        read_body(&mut reader, chunked, |piece| {
            body.extend_from_slice(piece);
            true
        })
        .unwrap();
        (status, String::from_utf8(body).unwrap())
    }

    #[test]
    fn plain_bodies() {
        assert_eq!(
            read_all("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}"),
            (200, "{\"ok\":true}".to_owned())
        );
    }

    #[test]
    fn chunked_bodies() {
        let response =
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n7;ext=1\r\n, world\r\n0\r\n\r\n";
        assert_eq!(read_all(response), (200, "hello, world".to_owned()));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(read_head(&mut Cursor::new("garbage\r\n\r\n".as_bytes())).is_err());
    }
}
