//! The router's presets from `models.ini`: each one's name, friendly alias and model file, and from the file's chat
//! template its thinking abilities. A setting `thinking.<preset> = switch | levels | always | none` in `corral.conf`
//! overrides what the template says, for templates that hide their controls.

use std::fs;
use std::path::PathBuf;

use crate::config::{self, Config, config_dir};
use crate::gguf;
use crate::thinking::Abilities;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    pub id: String,
    pub alias: Option<String>,
    pub model: Option<PathBuf>,
}

impl Preset {
    /// Whether an app asking for `name` means this preset (by its section name or its alias).
    pub fn is(&self, name: &str) -> bool {
        self.id == name || self.alias.as_deref() == Some(name)
    }

    /// The thinking abilities: the setting if there is one, otherwise read from the model file's template.
    pub fn abilities(&self, config: &Config) -> Result<Abilities, String> {
        if let Some(setting) = config.get(&format!("thinking.{}", self.id)) {
            return Abilities::from_setting(setting)
                .ok_or_else(|| format!("`thinking.{}` should be switch, levels, always or none", self.id));
        }
        let model = self.model.as_ref().ok_or("the preset has no model file")?;
        match gguf::chat_template(model) {
            Ok(Some(template)) => Ok(Abilities::of_template(&template)),
            Ok(None) => Ok(Abilities::default()),
            Err(error) => Err(format!("couldn't read {}: {error}", model.display())),
        }
    }
}

/// The presets in `models.ini`; `[*]` holds defaults, not a preset.
pub fn load() -> Vec<Preset> {
    parse(&fs::read_to_string(config_dir().join("models.ini")).unwrap_or_default())
}

pub fn parse(models_ini: &str) -> Vec<Preset> {
    let home = std::env::var("HOME").unwrap_or_default();
    config::sections(models_ini)
        .into_iter()
        .filter(|(id, _)| id != "*")
        .map(|(id, preset)| Preset {
            alias: preset.get("alias").map(str::to_owned),
            model: preset.get("model").map(|path| match path.strip_prefix("~/") {
                Some(rest) => PathBuf::from(&home).join(rest),
                None => PathBuf::from(path),
            }),
            id,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_with_aliases_and_files() {
        let presets = parse(
            "[*]\nctx-size = 8192\n\n[gemma]\nalias = Gemma 4\nmodel = /m/gemma.gguf\n\n# [old]\n# model = x\n\n[bare]\n",
        );
        assert_eq!(presets.len(), 2);
        assert!(presets[0].is("gemma") && presets[0].is("Gemma 4") && !presets[0].is("bare"));
        assert_eq!(presets[0].model, Some(PathBuf::from("/m/gemma.gguf")));
        assert_eq!(presets[1].model, None);
    }

    #[test]
    fn a_setting_overrides_the_template() {
        let preset = &parse("[odd]\nmodel = /does/not/exist.gguf\n")[0];
        let config = Config::parse("thinking.odd = always");
        assert_eq!(preset.abilities(&config).unwrap().describe(), "always thinks");
        assert!(preset.abilities(&Config::parse("")).is_err());
        assert!(preset.abilities(&Config::parse("thinking.odd = maybe")).is_err());
    }
}
