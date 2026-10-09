//! llama.cpp's router, asked on its own port so a question never starts it through the gate.

use std::collections::HashMap;
use std::io;

use serde_json::{Value, json};

use crate::http;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A preset changed state: `loading`, `loaded`, `sleeping`, `unloaded` or `failed`.
    State { model: String, state: String },
    /// The router reread its presets.
    Reload,
}

/// Every preset's model file by preset ID, from the arguments the router starts it with.
pub fn preset_files(port: u16) -> Option<HashMap<String, String>> {
    Some(parse_preset_files(&http::get_json(port, "/models")?))
}

/// Follows the router's live events until the stream ends (the router stopped) or `on_event` returns false.
pub fn follow_events(port: u16, mut on_event: impl FnMut(Event) -> bool) -> io::Result<()> {
    let mut buffer = String::new();
    http::request(port, "GET", "/models/sse", None, None, |piece| {
        buffer.push_str(&String::from_utf8_lossy(piece).replace("\r\n", "\n"));
        // Server-sent events: `data: {json}` records separated by a blank line
        while let Some(end) = buffer.find("\n\n") {
            let record: String = buffer.drain(..end + 2).collect();
            if let Some(event) = parse_event(&record)
                && !on_event(event)
            {
                return false;
            }
        }
        true
    })
    .map(|_| ())
}

/// Asks the router to unload a model; true if it agreed.
pub fn unload(port: u16, model: &str) -> bool {
    http::post_json(port, "/models/unload", &json!({ "model": model }))
}

fn parse_event(record: &str) -> Option<Event> {
    let data: String = record
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect();
    let event: Value = serde_json::from_str(&data).ok()?;
    if event["event"] == "models_reload" {
        return Some(Event::Reload);
    }
    Some(Event::State {
        model: event["model"].as_str()?.to_owned(),
        state: event.pointer("/data/status")?.as_str()?.to_owned(),
    })
}

fn parse_preset_files(json: &Value) -> HashMap<String, String> {
    let presets = json.get("data").and_then(Value::as_array).into_iter().flatten();
    presets
        .filter_map(|preset| {
            let args: Vec<&str> = preset
                .pointer("/status/args")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let file = args
                .iter()
                .position(|arg| *arg == "--model")
                .and_then(|i| args.get(i + 1))?;
            Some((preset.get("id")?.as_str()?.to_owned(), (*file).to_owned()))
        })
        .collect()
}

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
    parse_models(&http::get_json(port, "/models")?)
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
    fn preset_files_come_from_the_start_arguments() {
        let answer = json!({"data": [
            {"id": "a", "status": {"value": "unloaded", "args": ["llama-server", "--model", "/m/a.gguf", "--port", "0"]}},
            {"id": "b", "status": {"value": "unloaded", "args": ["llama-server"]}},
        ]});
        assert_eq!(
            parse_preset_files(&answer),
            HashMap::from([("a".to_owned(), "/m/a.gguf".to_owned())])
        );
    }

    #[test]
    fn events_from_the_stream() {
        assert_eq!(
            parse_event("data: {\"model\": \"a\", \"data\": {\"status\": \"loading\"}}\n\n"),
            Some(Event::State {
                model: "a".to_owned(),
                state: "loading".to_owned()
            })
        );
        assert_eq!(
            parse_event("data: {\"event\": \"models_reload\"}\n\n"),
            Some(Event::Reload)
        );
        assert_eq!(parse_event(": keep-alive\n\n"), None);
    }
}
