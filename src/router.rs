//! llama.cpp's router, asked on its own port so a question never starts it through the gate.

use serde_json::Value;

use crate::http;

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
}
