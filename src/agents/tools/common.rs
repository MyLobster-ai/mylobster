use serde_json::Value;
use std::collections::HashMap;

/// Read a required string parameter.
pub fn read_string_param(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .ok_or_else(|| format!("Missing required parameter: {}", key))
}

/// Read an optional string parameter.
pub fn read_optional_string_param(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read a string-or-number parameter as string.
pub fn read_string_or_number_param(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// Read an optional number parameter.
pub fn read_number_param(params: &Value, key: &str) -> Option<f64> {
    params.get(key).and_then(|v| v.as_f64())
}

/// Read an optional integer parameter.
pub fn read_integer_param(params: &Value, key: &str) -> Option<i64> {
    params.get(key).and_then(|v| v.as_i64())
}

/// Read an optional boolean parameter.
pub fn read_bool_param(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(|v| v.as_bool())
}

/// Read a string array parameter.
pub fn read_string_array_param(params: &Value, key: &str) -> Option<Vec<String>> {
    params.get(key).and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
    })
}

/// Create an action gate for conditional tool execution.
pub fn create_action_gate(
    actions: &Option<HashMap<String, bool>>,
) -> impl Fn(&str, bool) -> bool + '_ {
    move |key: &str, default: bool| -> bool {
        actions
            .as_ref()
            .and_then(|a| a.get(key))
            .copied()
            .unwrap_or(default)
    }
}
