//! The explicitly selected experimental policy. Execution and security settings are not policy.
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnPolicy {
    pub(crate) instruction: String,
    pub(crate) handoff_instruction: String,
    pub(crate) handoff_at: Option<u64>,
}

impl TurnPolicy {
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let obj = value.as_object().ok_or("policy must be an object")?;
        for key in obj.keys() {
            if !["instruction", "handoff_instruction", "handoff_at"].contains(&key.as_str()) {
                return Err(format!("{key} is outside the editable policy"));
            }
        }
        let text = |key: &str| -> Result<String, String> {
            let Some(value) = obj.get(key) else { return Ok(String::new()) };
            let s = value.as_str().ok_or_else(|| format!("{key} must be text"))?;
            if s.len() > 4096 || s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
                return Err(format!("{key} must be at most 4096 bytes without control characters"));
            }
            Ok(s.into())
        };
        let handoff_at = match obj.get("handoff_at") {
            None => None,
            Some(value) => Some(value.as_u64().filter(|n| (16_000..=128_000).contains(n))
                .ok_or("handoff_at must be an integer from 16000 through 128000")?),
        };
        Ok(Self { instruction: text("instruction")?, handoff_instruction: text("handoff_instruction")?, handoff_at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_policy_preserves_defaults_and_rejects_execution_controls() {
        assert_eq!(TurnPolicy::from_json(&json!({})).unwrap(), TurnPolicy::default());
        for value in [json!({"tools": []}), json!({"mode": "auto"}), json!({"network": true}),
                      json!({"handoff_at": 15999}), json!({"handoff_at": 128001}),
                      json!({"handoff_at": 64000.0}), json!({"instruction": "\u{1b}"}),
                      json!({"instruction": "\u{85}"}), json!({"instruction": "\u{9f}"}),
                      json!({"instruction": "x".repeat(4097)})] {
            assert!(TurnPolicy::from_json(&value).is_err(), "{value}");
        }
        assert!(TurnPolicy::from_json(&json!({"instruction": "Check your work.", "handoff_at": 16000})).is_ok());
    }
}
