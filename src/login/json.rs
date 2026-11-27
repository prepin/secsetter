use std::io;

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JsonCheck {
    matches: toml::Value,
    #[serde(default)]
    on_miss: OnMiss,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum OnMiss {
    #[default]
    Unknown,
    LoginNeeded,
}

impl JsonCheck {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_pattern(&self.matches)
    }

    pub fn evaluate(&self, output: &[u8]) -> io::Result<bool> {
        let value =
            serde_json::from_slice(output).map_err(|_| io::Error::other("invalid JSON output"))?;
        match matches_pattern(&self.matches, &value) {
            Some(true) => Ok(true),
            Some(false) if matches!(self.on_miss, OnMiss::LoginNeeded) => Ok(false),
            Some(false) => Err(io::Error::other("JSON pattern did not match")),
            None => Err(io::Error::other("unexpected JSON output shape")),
        }
    }
}

fn validate_pattern(pattern: &toml::Value) -> Result<(), &'static str> {
    match pattern {
        toml::Value::Table(fields) => fields.values().try_for_each(validate_pattern),
        toml::Value::Array(items) => match items.as_slice() {
            [item] => validate_pattern(item),
            _ => Err("JSON match arrays must contain exactly one pattern"),
        },
        toml::Value::Datetime(_) => Err("JSON match patterns cannot contain dates or times"),
        toml::Value::Float(value) if !value.is_finite() => Err("JSON match numbers must be finite"),
        _ => Ok(()),
    }
}

// None means the output does not have the expected shape.
fn matches_pattern(pattern: &toml::Value, value: &Value) -> Option<bool> {
    match pattern {
        toml::Value::Table(fields) => {
            let object = value.as_object()?;
            let mut result = Some(true);
            for (key, pattern) in fields {
                match object
                    .get(key)
                    .and_then(|value| matches_pattern(pattern, value))
                {
                    Some(false) => return Some(false),
                    None => result = None,
                    Some(true) => {}
                }
            }
            result
        }
        toml::Value::Array(items) => {
            let mut result = Some(false);
            for value in value.as_array()? {
                match matches_pattern(&items[0], value) {
                    Some(true) => return Some(true),
                    None => result = None,
                    Some(false) => {}
                }
            }
            result
        }
        toml::Value::String(expected) => Some(value.as_str()? == expected),
        toml::Value::Boolean(expected) => Some(value.as_bool()? == *expected),
        toml::Value::Integer(expected) => {
            let number = value.as_number().filter(|number| !number.is_f64())?;
            Some(number.as_i64() == Some(*expected))
        }
        toml::Value::Float(expected) => {
            let number = value.as_number().filter(|number| number.is_f64())?;
            Some(number.as_f64() == Some(*expected))
        }
        toml::Value::Datetime(_) => unreachable!("config validates JSON match patterns"),
    }
}
