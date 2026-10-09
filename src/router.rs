//! llama.cpp's router, asked on its own port so a question never starts it through the gate.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    /// The preset's friendly name (`alias` in `models.ini`), if it has one.
    pub alias: Option<String>,
    /// `loading`, `loaded` or `sleeping`.
    pub state: String,
}

impl Model {
    pub fn name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.id)
    }
}

/// The models that aren't unloaded, or `None` if the router doesn't answer.
pub fn active_models(port: u16) -> Option<Vec<Model>> {
    parse_models(&get_json(port, "/models")?)
}

/// A GET request to the router on this machine; `None` unless it answers 200 with JSON.
pub fn get_json(port: u16, path: &str) -> Option<Value> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, TIMEOUT).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    // HTTP/1.0: the server closes the connection after answering and never chunks the body
    write!(stream, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    parse_response(&response)
}

fn parse_response(response: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(response).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    if head.split_whitespace().nth(1)? != "200" {
        return None;
    }
    serde_json::from_str(body).ok()
}

fn parse_models(json: &Value) -> Option<Vec<Model>> {
    let models = json.get("data")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|model| {
                let state = model.pointer("/status/value")?.as_str()?;
                if state == "unloaded" {
                    return None;
                }
                Some(Model {
                    id: model.get("id")?.as_str()?.to_owned(),
                    alias: model.pointer("/aliases/0").and_then(Value::as_str).map(str::to_owned),
                    state: state.to_owned(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_models_that_are_not_unloaded() {
        let answer = json!({
            "object": "list",
            "data": [
                {"id": "a", "aliases": ["Model A"], "status": {"value": "unloaded"}},
                {"id": "b", "aliases": ["Model B"], "status": {"value": "loaded"}},
                {"id": "c", "aliases": [], "status": {"value": "sleeping"}},
            ]
        });
        let models = parse_models(&answer).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name(), "Model B");
        assert_eq!(models[1].name(), "c");
        assert_eq!(models[1].state, "sleeping");
    }

    #[test]
    fn answers_other_than_200_are_ignored() {
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}"),
            Some(json!({"ok": true}))
        );
        assert_eq!(parse_response(b"HTTP/1.1 503 Service Unavailable\r\n\r\n{}"), None);
        assert_eq!(parse_response(b"garbage"), None);
    }
}
