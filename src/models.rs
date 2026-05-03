// Loaded from models.toml. Edit `selected` + entries when new models ship.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize, Debug, Clone)]
pub struct ModelsConfig {
    pub selected: String,
    pub models: HashMap<String, ModelEntry>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ModelEntry {
    pub pi_arg: Option<String>,
    pub context_window_tokens: usize,
    pub compaction_threshold_pct: f32,
    pub keep_recent_tokens: usize,
    #[serde(default)]
    pub extra_pi_args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub name: String,
    pub pi_arg: Option<String>,
    pub context_window_tokens: usize,
    pub compaction_threshold_pct: f32,
    pub keep_recent_tokens: usize,
    pub extra_pi_args: Vec<String>,
}

impl ModelsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading models config at {}", path.display()))?;
        let parsed: ModelsConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing models config at {}", path.display()))?;
        Ok(parsed)
    }

    pub fn resolve(&self) -> Result<ResolvedModel> {
        let entry = self.models.get(&self.selected).with_context(|| {
            let mut available: Vec<&String> = self.models.keys().collect();
            available.sort();
            format!(
                "selected model `{}` not found in models config; available: {:?}",
                self.selected, available
            )
        })?;
        Ok(ResolvedModel {
            name: self.selected.clone(),
            pi_arg: entry.pi_arg.clone(),
            context_window_tokens: entry.context_window_tokens,
            compaction_threshold_pct: entry.compaction_threshold_pct,
            keep_recent_tokens: entry.keep_recent_tokens,
            extra_pi_args: entry.extra_pi_args.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_resolves_selected() {
        let raw = r#"
selected = "sonnet"

[models.sonnet]
pi_arg = "claude-sonnet-4-5"
context_window_tokens = 200000
compaction_threshold_pct = 0.7
keep_recent_tokens = 30000

[models.opus]
pi_arg = "claude-opus-4"
context_window_tokens = 200000
compaction_threshold_pct = 0.7
keep_recent_tokens = 30000
"#;
        let cfg: ModelsConfig = toml::from_str(raw).unwrap();
        let resolved = cfg.resolve().unwrap();
        assert_eq!(resolved.name, "sonnet");
        assert_eq!(resolved.pi_arg.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(resolved.context_window_tokens, 200000);
    }

    #[test]
    fn missing_selected_errors_helpfully() {
        let raw = r#"
selected = "haiku"

[models.sonnet]
pi_arg = "claude-sonnet-4-5"
context_window_tokens = 200000
compaction_threshold_pct = 0.7
keep_recent_tokens = 30000
"#;
        let cfg: ModelsConfig = toml::from_str(raw).unwrap();
        let err = cfg.resolve().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("haiku"), "{msg}");
        assert!(msg.contains("sonnet"), "{msg}");
    }

    #[test]
    fn extra_pi_args_default_to_empty() {
        let raw = r#"
selected = "x"

[models.x]
pi_arg = "claude-x"
context_window_tokens = 100000
compaction_threshold_pct = 0.7
keep_recent_tokens = 10000
"#;
        let cfg: ModelsConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.resolve().unwrap().extra_pi_args, Vec::<String>::new());
    }
}
