//! Checking a tool call's `arguments` against the `inputSchema` the tool publishes.
//!
//! **ONLY THE KEYWORDS THE CONTRACT USES, AND A TEST HOLDS IT TO THAT.** This is not a JSON Schema
//! implementation. It enforces the handful of keywords `tools.json` writes, listed in [`CHECKED`],
//! and a test fails the day the contract uses a keyword or a type this does not know, so a schema
//! can never promise a constraint that nothing checks.

use serde_json::Value;

/// The keywords [`check`] enforces. Kept beside it, so a keyword it gains is added here in the same
/// change, and read by the test that holds the contract to this list.
#[cfg(test)]
pub const CHECKED: [&str; 7] =
    ["type", "properties", "required", "enum", "items", "additionalProperties", "minimum"];

/// The keywords that describe a value and constrain nothing.
#[cfg(test)]
pub const ANNOTATIONS: [&str; 3] = ["description", "default", "title"];

/// The values of `type` [`check`] knows.
#[cfg(test)]
pub const TYPES: [&str; 7] = ["object", "array", "string", "boolean", "integer", "number", "null"];

/// Why `arguments` does not satisfy `schema`, naming the argument at fault, or `Ok`.
pub fn check(arguments: &Value, schema: &Value) -> Result<(), String> {
    check_at(arguments, schema, "arguments")
}

fn check_at(value: &Value, schema: &Value, at: &str) -> Result<(), String> {
    if let Some(kind) = schema.get("type").and_then(Value::as_str)
        && !is_kind(value, kind)
    {
        return Err(format!("{at} must be {}", phrase(kind)));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        let listed: Vec<String> = allowed.iter().map(Value::to_string).collect();
        return Err(format!("{at} must be one of {}", listed.join(", ")));
    }
    if let Some(minimum) = schema.get("minimum")
        && below(value, minimum)
    {
        return Err(format!("{at} must be {minimum} or more"));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{at}.{name} is required"));
                }
            }
        }
        let declared = schema.get("properties").and_then(Value::as_object);
        for (name, field) in object {
            let place = format!("{at}.{name}");
            match declared.and_then(|properties| properties.get(name)) {
                Some(property) => check_at(field, property, &place)?,
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        return Err(format!("{place} is not an argument this tool takes"));
                    }
                    Some(extra @ Value::Object(_)) => check_at(field, extra, &place)?,
                    _ => {}
                },
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (index, item) in array.iter().enumerate() {
            check_at(item, items, &format!("{at}[{index}]"))?;
        }
    }
    Ok(())
}

/// Whether `value` is a JSON value of the schema type `kind`. A type this does not know holds
/// nothing, so it refuses rather than passing everything.
fn is_kind(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// How a refusal names the type `kind`.
fn phrase(kind: &str) -> String {
    match kind {
        "object" => "an object".to_owned(),
        "array" => "an array".to_owned(),
        "boolean" => "true or false".to_owned(),
        "integer" => "an integer".to_owned(),
        "null" => "null".to_owned(),
        other => format!("a {other}"),
    }
}

/// Whether the number `value` is below `minimum`. A value that is not a number is not below
/// anything: its type is `type`'s to refuse.
fn below(value: &Value, minimum: &Value) -> bool {
    match (value.as_i64(), minimum.as_i64()) {
        (Some(value), Some(minimum)) => value < minimum,
        _ => matches!((value.as_f64(), minimum.as_f64()), (Some(value), Some(minimum)) if value < minimum),
    }
}

#[cfg(test)]
mod tests {
    use super::check;
    use serde_json::json;

    /// Each keyword refuses what it rules out and passes what it allows, and a refusal names the
    /// argument at fault by its path.
    #[test]
    fn each_keyword_refuses_what_it_rules_out_naming_the_argument() {
        let schema = json!({
            "type": "object",
            "properties": {
                "code": { "type": "string" },
                "image_bytes": { "type": "integer", "minimum": 0 },
                "via": { "type": "string", "enum": ["volume", "probe"] },
                "lines": { "type": "array", "items": { "type": "integer" } },
                "readings": { "type": "object", "additionalProperties": { "type": "integer" } },
                "run": { "type": "boolean", "default": true }
            },
            "required": ["code"]
        });
        assert!(check(&json!({ "code": "x" }), &schema).is_ok());
        let everything = json!({
            "code": "x", "image_bytes": 0, "via": "probe", "lines": [1, 2],
            "readings": { "a": 3 }, "run": false, "unlisted": "an extra argument is allowed"
        });
        assert!(check(&everything, &schema).is_ok());

        let refused = |arguments| check(&arguments, &schema).expect_err("refused");
        assert_eq!(refused(json!({})), "arguments.code is required");
        assert_eq!(refused(json!([])), "arguments must be an object");
        assert_eq!(refused(json!({ "code": 7 })), "arguments.code must be a string");
        assert_eq!(refused(json!({ "code": "x", "image_bytes": -1 })), "arguments.image_bytes must be 0 or more");
        assert_eq!(refused(json!({ "code": "x", "image_bytes": 1.5 })), "arguments.image_bytes must be an integer");
        assert_eq!(
            refused(json!({ "code": "x", "via": "usb" })),
            "arguments.via must be one of \"volume\", \"probe\""
        );
        assert_eq!(refused(json!({ "code": "x", "lines": [1, "2"] })), "arguments.lines[1] must be an integer");
        assert_eq!(
            refused(json!({ "code": "x", "readings": { "a": "3" } })),
            "arguments.readings.a must be an integer"
        );
        assert_eq!(refused(json!({ "code": "x", "run": "yes" })), "arguments.run must be true or false");
    }

    /// `additionalProperties: false` refuses an argument the schema does not declare.
    #[test]
    fn a_closed_schema_refuses_an_argument_it_does_not_declare() {
        let closed = json!({ "type": "object", "additionalProperties": false });
        assert!(check(&json!({}), &closed).is_ok());
        assert_eq!(
            check(&json!({ "surprise": 1 }), &closed).expect_err("refused"),
            "arguments.surprise is not an argument this tool takes"
        );
    }
}
