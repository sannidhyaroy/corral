//! The thinking translator's logic. Apps send the standard `reasoning_effort` (`none`, `minimal`, `low`, `medium`,
//! `high`, `xhigh`, `max`), which llama.cpp mostly ignores; each model understands something else: an on/off switch
//! in its template, effort levels in its template, or nothing, thinking always. This module reads which from the
//! model's chat template and rewrites a request into what that model understands. Native behaviour is used when the
//! model has it; a token cap only stands in for "off", never for an effort level.

use serde_json::{Map, Value, json};

/// What a model's template offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Abilities {
    /// An on/off switch (`enable_thinking`).
    pub switch: bool,
    /// Effort levels the model was trained on (`reasoning_effort`, low/medium/high).
    pub levels: bool,
    /// It thinks at all.
    pub thinks: bool,
}

impl Abilities {
    /// Reads the abilities from a chat template.
    pub fn of_template(template: &str) -> Self {
        let switch = template.contains("enable_thinking");
        let levels = template.contains("reasoning_effort");
        let thinks = switch || levels || template.contains("think>");
        Self { switch, levels, thinks }
    }

    /// A setting's answer, for templates that need it: `switch`, `levels`, `always` or `none`.
    pub fn from_setting(setting: &str) -> Option<Self> {
        match setting {
            "switch" => Some(Self {
                switch: true,
                levels: false,
                thinks: true,
            }),
            "levels" => Some(Self {
                switch: false,
                levels: true,
                thinks: true,
            }),
            "always" => Some(Self {
                switch: false,
                levels: false,
                thinks: true,
            }),
            "none" => Some(Self::default()),
            _ => None,
        }
    }

    /// A short name for listings.
    pub fn describe(self) -> &'static str {
        match (self.thinks, self.switch, self.levels) {
            (false, _, _) => "no thinking",
            (true, true, true) => "switch and levels",
            (true, true, false) => "switch",
            (true, false, true) => "levels",
            (true, false, false) => "always thinks",
        }
    }
}

/// The standard effort as a level a levels model knows: its top level for anything above `high`.
fn level(effort: &str) -> Option<&'static str> {
    match effort {
        "minimal" | "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// Rewrites a chat request's standard `reasoning_effort` into what a model with these abilities understands.
/// Requests that already set the model's own fields (`chat_template_kwargs.enable_thinking` or
/// `.reasoning_effort`, or `thinking_budget_tokens`) are left alone, so a client that knows the model, like the web
/// UI, keeps full control. Returns what was done, for the log, or `None` if nothing was.
pub fn translate(request: &mut Value, abilities: Abilities) -> Option<String> {
    let body = request.as_object_mut()?;
    let effort = body.get("reasoning_effort")?.as_str()?.to_ascii_lowercase();
    let own = |key: &str| {
        body.get("chat_template_kwargs")
            .and_then(|kwargs| kwargs.get(key))
            .is_some()
    };
    if own("enable_thinking") || own("reasoning_effort") || body.contains_key("thinking_budget_tokens") {
        return None;
    }
    let off = effort == "none";
    if !off && level(&effort).is_none() {
        return None;
    }
    body.remove("reasoning_effort");

    let mut kwargs = |key: &str, value: Value| {
        let entry = body
            .entry("chat_template_kwargs")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(kwargs) = entry.as_object_mut() {
            kwargs.insert(key.to_owned(), value);
        }
    };
    let done = if !abilities.thinks {
        format!("{effort}: removed, the model doesn't think")
    } else if abilities.switch {
        kwargs("enable_thinking", json!(!off));
        if !off && abilities.levels {
            kwargs("reasoning_effort", json!(level(&effort)));
        }
        format!("{effort}: thinking switched {}", if off { "off" } else { "on" })
    } else if off {
        body.insert("thinking_budget_tokens".to_owned(), json!(0));
        format!("{effort}: thinking capped at 0 tokens (the model has no off switch)")
    } else if abilities.levels {
        let level = level(&effort).expect("checked above");
        kwargs("reasoning_effort", json!(level));
        format!("{effort}: the model's {level} level")
    } else {
        format!("{effort}: unchanged, the model always thinks")
    };
    Some(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SWITCH: Abilities = Abilities {
        switch: true,
        levels: false,
        thinks: true,
    };
    const LEVELS: Abilities = Abilities {
        switch: false,
        levels: true,
        thinks: true,
    };
    const ALWAYS: Abilities = Abilities {
        switch: false,
        levels: false,
        thinks: true,
    };
    const NONE: Abilities = Abilities {
        switch: false,
        levels: false,
        thinks: false,
    };

    fn request(effort: &str) -> Value {
        json!({"model": "m", "messages": [], "reasoning_effort": effort})
    }

    fn translated(effort: &str, abilities: Abilities) -> Value {
        let mut request = request(effort);
        translate(&mut request, abilities);
        request
    }

    #[test]
    fn abilities_come_from_the_template() {
        assert_eq!(
            Abilities::of_template("{% if enable_thinking %}<think>{% endif %}"),
            SWITCH
        );
        assert_eq!(Abilities::of_template("Reasoning: {{ reasoning_effort }}"), LEVELS);
        assert_eq!(Abilities::of_template("{{ content.split('</think>')[-1] }}"), ALWAYS);
        assert_eq!(Abilities::of_template("{{ messages }}"), NONE);
    }

    #[test]
    fn switch_models_use_their_switch() {
        assert_eq!(
            translated("none", SWITCH)["chat_template_kwargs"]["enable_thinking"],
            false
        );
        for effort in ["low", "high", "max"] {
            let request = translated(effort, SWITCH);
            assert_eq!(request["chat_template_kwargs"]["enable_thinking"], true, "{effort}");
            assert!(request.get("reasoning_effort").is_none());
        }
    }

    #[test]
    fn levels_models_use_their_levels_and_a_cap_for_off() {
        assert_eq!(translated("none", LEVELS)["thinking_budget_tokens"], 0);
        assert_eq!(
            translated("minimal", LEVELS)["chat_template_kwargs"]["reasoning_effort"],
            "low"
        );
        assert_eq!(
            translated("medium", LEVELS)["chat_template_kwargs"]["reasoning_effort"],
            "medium"
        );
        assert_eq!(
            translated("xhigh", LEVELS)["chat_template_kwargs"]["reasoning_effort"],
            "high"
        );
        assert_eq!(
            translated("max", LEVELS)["chat_template_kwargs"]["reasoning_effort"],
            "high"
        );
    }

    #[test]
    fn always_thinking_models_only_change_for_off() {
        assert_eq!(translated("none", ALWAYS)["thinking_budget_tokens"], 0);
        let high = translated("high", ALWAYS);
        assert!(high.get("thinking_budget_tokens").is_none() && high.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn models_without_thinking_just_lose_the_field() {
        let request = translated("high", NONE);
        assert!(request.get("reasoning_effort").is_none() && request.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn requests_that_set_the_models_own_fields_are_left_alone() {
        for mut request in [
            json!({"reasoning_effort": "none", "chat_template_kwargs": {"enable_thinking": true}}),
            json!({"reasoning_effort": "low", "thinking_budget_tokens": 512}),
            json!({"messages": []}),
            json!({"reasoning_effort": "unheard-of"}),
        ] {
            let before = request.clone();
            assert_eq!(translate(&mut request, SWITCH), None);
            assert_eq!(request, before);
        }
    }

    #[test]
    fn existing_template_arguments_are_kept() {
        let mut request = json!({"reasoning_effort": "high", "chat_template_kwargs": {"keep": 1}});
        translate(&mut request, SWITCH);
        assert_eq!(
            request["chat_template_kwargs"],
            json!({"keep": 1, "enable_thinking": true})
        );
    }
}
