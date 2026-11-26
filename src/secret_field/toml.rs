use toml_edit::{DocumentMut, Item, TableLike, Value};

use super::EditError;

pub(super) fn is_set(source: String, key: &[String]) -> Result<bool, &'static str> {
    let mut document = parse(&source)?;
    match target(&mut document, key) {
        Ok((table, name)) => Ok(field(table, name)?
            .is_some_and(|value| value.as_str().is_some_and(|string| !string.is_empty()))),
        Err(EditError::MissingParent(_)) => Ok(false),
        Err(EditError::Blocked(reason)) => Err(reason),
    }
}

pub(super) fn edit(source: String, key: &[String], value: &str) -> Result<String, EditError> {
    let mut document = parse(&source).map_err(EditError::Blocked)?;
    let (table, name) = target(&mut document, key)?;
    let mut value = Value::from(value);
    if let Some(old) = field(table, name).map_err(EditError::Blocked)? {
        *value.decor_mut() = old.decor().clone();
        *old = value;
    } else {
        table.insert(name, Item::Value(value));
    }
    Ok(document.to_string())
}

fn parse(source: &str) -> Result<DocumentMut, &'static str> {
    source.parse::<DocumentMut>().map_err(|error| {
        if error.message().starts_with("duplicate key") {
            "ambiguous TOML document: duplicate key"
        } else {
            "invalid TOML document"
        }
    })
}

fn target<'doc, 'key>(
    document: &'doc mut DocumentMut,
    key: &'key [String],
) -> Result<(&'doc mut dyn TableLike, &'key str), EditError> {
    let (name, parents) = key
        .split_last()
        .expect("config requires a non-empty key path");
    let mut table: &mut dyn TableLike = document.as_table_mut();
    for (index, parent) in parents.iter().enumerate() {
        table = table
            .get_mut(parent)
            .ok_or(EditError::MissingParent(index + 1))?
            .as_table_like_mut()
            .ok_or(EditError::Blocked("expected a TOML table on the key path"))?;
    }
    Ok((table, name))
}

fn field<'a>(
    table: &'a mut dyn TableLike,
    name: &str,
) -> Result<Option<&'a mut Value>, &'static str> {
    table
        .get_mut(name)
        .map(|item| match item {
            Item::Value(value @ Value::String(_)) => Ok(value),
            _ => Err("expected a TOML string at the selected field"),
        })
        .transpose()
}
