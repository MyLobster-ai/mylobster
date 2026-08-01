//! Config RPC helpers (v2026.5.2 parity: "Cap oversized plugin-owned
//! schemas in `config.schema` response").
//!
//! Plugin-owned subschemas can be arbitrarily large; capping them keeps the
//! `config.schema` response bounded for UI/SDK consumers.

/// Maximum serialized bytes allowed for a single plugin-owned schema entry
/// in the `config.schema` response.
pub const MAX_PLUGIN_SCHEMA_BYTES: usize = 64 * 1024;

/// Placeholder inserted for a capped plugin schema.
pub fn truncated_schema_placeholder(original_bytes: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "truncated": true,
        "originalBytes": original_bytes,
        "description": "plugin schema exceeded the response size cap and was truncated",
    })
}

/// Cap oversized plugin-owned schema entries in a `config.schema` response.
///
/// `schema` is the full response schema object; plugin-owned entries live
/// under `properties.plugins.properties.<pluginId>` (and any object entry
/// tagged with `"x-plugin": true`). Returns the number of capped entries.
pub fn cap_plugin_schemas(schema: &mut serde_json::Value, max_bytes: usize) -> usize {
    let mut capped = 0;

    // Explicit plugin schema container.
    if let Some(plugin_props) = schema
        .pointer_mut("/properties/plugins/properties")
        .and_then(|v| v.as_object_mut())
    {
        for (_id, sub) in plugin_props.iter_mut() {
            capped += cap_if_oversized(sub, max_bytes);
        }
    }

    // Any object entry tagged x-plugin anywhere at the top two levels.
    if let Some(props) = schema
        .pointer_mut("/properties")
        .and_then(|v| v.as_object_mut())
    {
        for (_k, sub) in props.iter_mut() {
            let tagged = sub
                .get("x-plugin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if tagged {
                capped += cap_if_oversized(sub, max_bytes);
            }
        }
    }

    capped
}

fn cap_if_oversized(schema: &mut serde_json::Value, max_bytes: usize) -> usize {
    let size = serde_json::to_string(schema).map(|s| s.len()).unwrap_or(0);
    if size > max_bytes {
        *schema = truncated_schema_placeholder(size);
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn big_schema(bytes: usize) -> serde_json::Value {
        json!({
            "type": "object",
            "description": "x".repeat(bytes),
        })
    }

    #[test]
    fn small_plugin_schemas_untouched() {
        let mut schema = json!({
            "properties": {
                "plugins": {
                    "properties": {
                        "small": {"type": "object"}
                    }
                }
            }
        });
        assert_eq!(cap_plugin_schemas(&mut schema, 1024), 0);
        assert_eq!(
            schema["properties"]["plugins"]["properties"]["small"]["type"],
            "object"
        );
    }

    #[test]
    fn oversized_plugin_schema_capped() {
        let mut schema = json!({
            "properties": {
                "plugins": {
                    "properties": {
                        "huge": big_schema(5000),
                        "small": {"type": "object"}
                    }
                }
            }
        });
        assert_eq!(cap_plugin_schemas(&mut schema, 1024), 1);
        let huge = &schema["properties"]["plugins"]["properties"]["huge"];
        assert_eq!(huge["truncated"], true);
        assert!(huge["originalBytes"].as_u64().unwrap() > 1024);
        assert_eq!(
            schema["properties"]["plugins"]["properties"]["small"]["type"],
            "object"
        );
    }

    #[test]
    fn x_plugin_tagged_entries_capped() {
        let mut tagged = big_schema(5000);
        tagged["x-plugin"] = json!(true);
        let mut schema = json!({
            "properties": {
                "someExtension": tagged,
                "core": big_schema(5000),
            }
        });
        assert_eq!(cap_plugin_schemas(&mut schema, 1024), 1);
        assert_eq!(schema["properties"]["someExtension"]["truncated"], true);
        // Core (untagged) schemas are never capped.
        assert!(schema["properties"]["core"].get("truncated").is_none());
    }

    #[test]
    fn missing_plugins_section_is_noop() {
        let mut schema = json!({"properties": {"gateway": {"type": "object"}}});
        assert_eq!(cap_plugin_schemas(&mut schema, 1024), 0);
    }

    #[test]
    fn placeholder_shape() {
        let p = truncated_schema_placeholder(999);
        assert_eq!(p["type"], "object");
        assert_eq!(p["truncated"], true);
        assert_eq!(p["originalBytes"], 999);
    }
}
