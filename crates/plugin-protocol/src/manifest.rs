use std::{collections::BTreeMap, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CURRENT_PROTOCOL_VERSION, PluginCapabilities};

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct PluginManifest {
    #[serde(default = "manifest_version")]
    pub manifest_version: u32,
    pub name: String,
    pub version: String,
    #[serde(default = "supported_protocol_versions")]
    pub protocol_versions: Vec<u32>,
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub capabilities: PluginCapabilities,
}

const fn manifest_version() -> u32 {
    MANIFEST_VERSION
}

fn supported_protocol_versions() -> Vec<u32> {
    vec![CURRENT_PROTOCOL_VERSION]
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("unsupported plugin manifest version {0}")]
    UnsupportedManifestVersion(u32),
    #[error(
        "plugin name must start with an ASCII letter and contain only lowercase letters, digits, '-' or '_'"
    )]
    InvalidName,
    #[error("plugin version cannot be empty")]
    EmptyVersion,
    #[error("plugin executable cannot be empty")]
    EmptyExecutable,
    #[error("plugin must advertise at least one non-zero protocol version")]
    InvalidProtocolVersions,
    #[error("plugin environment key is invalid: {0}")]
    InvalidEnvironmentKey(String),
    #[error("failed to parse plugin manifest: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to encode plugin manifest: {0}")]
    Encode(#[from] toml::ser::Error),
}

impl PluginManifest {
    pub fn from_toml(input: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_toml(&self) -> Result<String, ManifestError> {
        self.validate()?;
        toml::to_string_pretty(self).map_err(ManifestError::from)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.manifest_version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedManifestVersion(
                self.manifest_version,
            ));
        }
        let mut chars = self.name.chars();
        if !chars.next().is_some_and(|first| first.is_ascii_lowercase())
            || !chars.all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '-' | '_')
            })
        {
            return Err(ManifestError::InvalidName);
        }
        if self.version.trim().is_empty() {
            return Err(ManifestError::EmptyVersion);
        }
        if self.executable.as_os_str().is_empty() {
            return Err(ManifestError::EmptyExecutable);
        }
        if self.protocol_versions.is_empty()
            || self.protocol_versions.contains(&0)
            || has_duplicates(&self.protocol_versions)
        {
            return Err(ManifestError::InvalidProtocolVersions);
        }
        if let Some(key) = self
            .env
            .keys()
            .find(|key| !valid_environment_key(key.as_str()))
        {
            return Err(ManifestError::InvalidEnvironmentKey(key.clone()));
        }
        Ok(())
    }
}

fn has_duplicates(values: &[u32]) -> bool {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).any(|pair| pair[0] == pair[1])
}

fn valid_environment_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        PluginManifest {
            manifest_version: MANIFEST_VERSION,
            name: "example-agent".into(),
            version: "1.0.0".into(),
            protocol_versions: vec![1],
            executable: "agentctl-provider-example".into(),
            args: vec![],
            env: BTreeMap::new(),
            capabilities: PluginCapabilities::default(),
        }
    }

    #[test]
    fn manifest_round_trips_through_toml() {
        let encoded = manifest().to_toml().unwrap();
        let decoded = PluginManifest::from_toml(&encoded).unwrap();
        assert_eq!(decoded.name, "example-agent");
        assert_eq!(decoded.protocol_versions, vec![1]);
    }

    #[test]
    fn unsafe_or_ambiguous_identifiers_are_rejected() {
        let mut value = manifest();
        value.name = "../../Agent".into();
        assert!(matches!(value.validate(), Err(ManifestError::InvalidName)));
        value.name = "valid".into();
        value.env.insert("BAD=KEY".into(), "value".into());
        assert!(matches!(
            value.validate(),
            Err(ManifestError::InvalidEnvironmentKey(_))
        ));
    }
}
