//! Resource provenance metadata — port of `core/source-info.ts`.
//!
//! `SourceInfo` is wire surface: RPC `get_commands` serializes it verbatim
//! for every extension command, prompt template, and skill.

use serde::{Deserialize, Serialize};

/// Oracle `SourceScope` (`"user" | "project" | "temporary"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceScope {
    User,
    Project,
    /// Oracle synthetic default.
    #[default]
    Temporary,
}

/// Oracle `SourceOrigin` (`"package" | "top-level"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceOrigin {
    Package,
    /// Oracle synthetic default.
    #[default]
    TopLevel,
}

/// Oracle `SourceInfo` (field order is the wire order).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

impl SourceInfo {
    /// Oracle `createSyntheticSourceInfo`: scope defaults to `temporary`,
    /// origin to `top-level`.
    pub fn synthetic(
        path: impl Into<String>,
        source: impl Into<String>,
        scope: Option<SourceScope>,
        origin: Option<SourceOrigin>,
        base_dir: Option<String>,
    ) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
            scope: scope.unwrap_or_default(),
            origin: origin.unwrap_or_default(),
            base_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_matches_oracle_key_order_and_values() {
        let info = SourceInfo::synthetic(
            "/tmp/skills/x/SKILL.md",
            "local",
            Some(SourceScope::User),
            None,
            Some("/tmp/skills/x".to_string()),
        );
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(
            json,
            r#"{"path":"/tmp/skills/x/SKILL.md","source":"local","scope":"user","origin":"top-level","baseDir":"/tmp/skills/x"}"#
        );
    }

    #[test]
    fn synthetic_defaults_match_oracle() {
        let info = SourceInfo::synthetic("p", "s", None, None, None);
        assert_eq!(info.scope, SourceScope::Temporary);
        assert_eq!(info.origin, SourceOrigin::TopLevel);
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("baseDir"));
        assert!(json.contains(r#""scope":"temporary""#));
        assert!(json.contains(r#""origin":"top-level""#));
    }
}
