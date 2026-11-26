use std::collections::HashMap;
use std::fmt;

use serde::de::{Deserialize, Deserializer, Error as _, MapAccess, Visitor};
use serde_json::value::RawValue;

use super::EditError;

const INVALID_DOCUMENT: &str = "invalid JSON document";
const DUPLICATE_KEY: &str = "ambiguous JSON document: duplicate key";

enum Node<'a> {
    Object(&'a str, HashMap<String, Node<'a>>),
    String(&'a str, bool),
    Other,
}

enum Target<'a> {
    AddTo { object: &'a str, has_fields: bool },
    Replace { token: &'a str, is_set: bool },
}

pub(super) fn is_set(source: String, key: &[String]) -> Result<bool, &'static str> {
    match target(&source, key) {
        Ok(target) => Ok(matches!(target, Target::Replace { is_set: true, .. })),
        Err(EditError::MissingParent(_)) => Ok(false),
        Err(EditError::Blocked(reason)) => Err(reason),
    }
}

pub(super) fn edit(mut source: String, key: &[String], value: &str) -> Result<String, EditError> {
    let target = target(&source, key)?;
    let encode = |string: &str| {
        serde_json::to_string(string)
            .map_err(|_| EditError::Blocked("could not encode a JSON string"))
    };
    let value = encode(value)?;
    // RawValue borrows the original text, so the token's address gives its byte offset.
    let offset = |token: &str| token.as_ptr().addr() - source.as_ptr().addr();
    match target {
        Target::Replace { token, .. } => {
            let start = offset(token);
            let end = start + token.len();
            source.replace_range(start..end, &value);
        }
        Target::AddTo { object, has_fields } => {
            let position = offset(object) + object[..object.len() - 1].trim_end().len();
            let comma = if has_fields { "," } else { "" };
            let name = encode(key.last().unwrap())?;
            source.insert_str(position, &format!("{comma}{name}:{value}"));
        }
    }
    Ok(source)
}

fn target<'a>(source: &'a str, key: &[String]) -> Result<Target<'a>, EditError> {
    let raw = serde_json::from_str(source).map_err(|_| EditError::Blocked(INVALID_DOCUMENT))?;
    let mut node = parse_node(raw, 0).map_err(EditError::Blocked)?;
    for (index, name) in key.iter().enumerate() {
        let Node::Object(object, mut fields) = node else {
            return Err(EditError::Blocked("expected a JSON object on the key path"));
        };
        let last = index + 1 == key.len();
        let Some(field) = fields.remove(name) else {
            return if last {
                Ok(Target::AddTo {
                    object,
                    has_fields: !fields.is_empty(),
                })
            } else {
                Err(EditError::MissingParent(index + 1))
            };
        };
        if last {
            return match field {
                Node::String(token, is_set) => Ok(Target::Replace { token, is_set }),
                _ => Err(EditError::Blocked(
                    "expected a JSON string at the selected field",
                )),
            };
        }
        node = field;
    }
    unreachable!("config requires a non-empty key path")
}

fn parse_node(raw: &RawValue, depth: usize) -> Result<Node<'_>, &'static str> {
    // RawValue skips nested values, so enforce serde_json's usual depth limit here.
    if depth == 128 {
        return Err("JSON document exceeds the nesting limit");
    }
    let token = raw.get();
    if token.starts_with('{') {
        let Object(fields) = serde_json::from_str(token).map_err(|error| {
            if error.is_data() {
                DUPLICATE_KEY
            } else {
                INVALID_DOCUMENT
            }
        })?;
        let fields = fields
            .into_iter()
            .map(|(name, raw)| Ok((name, parse_node(raw, depth + 1)?)))
            .collect::<Result<_, _>>()?;
        Ok(Node::Object(token, fields))
    } else if token.starts_with('[') {
        let items: Vec<&RawValue> = serde_json::from_str(token).map_err(|_| INVALID_DOCUMENT)?;
        for item in items {
            parse_node(item, depth + 1)?;
        }
        Ok(Node::Other)
    } else if token.starts_with('"') {
        let string: String = serde_json::from_str(token).map_err(|_| INVALID_DOCUMENT)?;
        Ok(Node::String(token, !string.is_empty()))
    } else {
        Ok(Node::Other)
    }
}

struct Object<'a>(HashMap<String, &'a RawValue>);

impl<'de> Deserialize<'de> for Object<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ReadObject;

        impl<'de> Visitor<'de> for ReadObject {
            type Value = Object<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut fields = HashMap::new();
                while let Some((name, raw)) = map.next_entry()? {
                    if fields.insert(name, raw).is_some() {
                        return Err(A::Error::custom(DUPLICATE_KEY));
                    }
                }
                Ok(Object(fields))
            }
        }

        deserializer.deserialize_map(ReadObject)
    }
}
