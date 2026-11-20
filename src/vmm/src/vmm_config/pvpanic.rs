// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// Host action requested when the guest reports a panic through pvpanic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PvPanicAction {
    /// Report the panic without pausing guest execution.
    #[default]
    None,
    /// Pause guest execution after reporting the panic.
    Pause,
}

/// Configuration of the pvpanic device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PvPanicConfig {
    /// Action requested when the guest reports `PVPANIC_PANICKED`.
    #[serde(default)]
    pub panic_action: PvPanicAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults_and_round_trip() {
        let default: PvPanicConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(default, PvPanicConfig::default());
        assert_eq!(default.panic_action, PvPanicAction::None);

        for (action, name) in [
            (PvPanicAction::None, "none"),
            (PvPanicAction::Pause, "pause"),
        ] {
            let json = format!(r#"{{"panic_action":"{name}"}}"#);
            let config: PvPanicConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(config.panic_action, action);
            assert_eq!(serde_json::to_string(&config).unwrap(), json);
        }
    }

    #[test]
    fn test_config_rejects_invalid_fields_and_actions() {
        for json in [
            r#"{"panic_action":"shutdown"}"#,
            r#"{"panic_action":"Pause"}"#,
            r#"{"panic_action":null}"#,
            r#"{"panic_action":1}"#,
            r#"{"unknown":true}"#,
        ] {
            serde_json::from_str::<PvPanicConfig>(json).unwrap_err();
        }
    }
}
