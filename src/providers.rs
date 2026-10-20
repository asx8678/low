use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

pub type Providers = BTreeMap<String, Provider>;

/// What to write when there is no provider yet.
pub fn example(model: &str) -> String {
    format!(
        "put a provider in ~/.config/low/providers.toml, for example:\n\n[myprovider]\nbase_url = \"https://…/v1\"\n\
         models = [\"{model}\"]"
    )
}

/// low's config directory.
pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config/low")
}

pub fn load(home: &Path) -> Result<Option<Providers>> {
    let path = config_dir(home).join("providers.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(toml::from_str(&text).with_context(|| path.display().to_string())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context(path.display().to_string()),
    }
}

pub fn key(provider: &Provider) -> &'static str {
    match provider.api_key.as_deref() {
        Some("") => "no key",
        Some(_) => "own key",
        None => "shared key",
    }
}

/// Whether reqwest can send to this base url: http or https, with a host.
pub fn web_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
}
