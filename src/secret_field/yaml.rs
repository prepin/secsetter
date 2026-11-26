use std::borrow::Cow;

use oneyaml::{Document, NodeId, NodeKind, NodeRef, ScalarStyle, Settings, code};

use super::EditError;

const INVALID_EDIT: &str = "could not safely edit the YAML field";

enum Target<'a> {
    MissingParent(usize),
    AddTo(NodeId),
    Replace(NodeRef<'a>, Cow<'a, str>),
}

pub(super) fn is_set(source: String, key: &[String]) -> Result<bool, &'static str> {
    let document = parse(source)?;
    Ok(matches!(target(&document, key)?, Target::Replace(_, value) if !value.is_empty()))
}

pub(super) fn edit(source: String, key: &[String], value: &str) -> Result<String, EditError> {
    let mut document = parse(source).map_err(EditError::Blocked)?;
    let invalid_edit = |_| EditError::Blocked(INVALID_EDIT);
    let edited = match target(&document, key).map_err(EditError::Blocked)? {
        Target::MissingParent(count) => return Err(EditError::MissingParent(count)),
        Target::AddTo(id) => {
            document
                .node_mut(id)
                .map_err(invalid_edit)?
                .insert(key.last().unwrap().as_str(), value)
                .map_err(invalid_edit)?;
            document.to_string()
        }
        Target::Replace(node, _)
            if node.as_scalar().is_some_and(|scalar| {
                matches!(scalar.style(), ScalarStyle::Literal | ScalarStyle::Folded)
            }) =>
        {
            replace_block(&document, node, value).map_err(EditError::Blocked)?
        }
        Target::Replace(node, _) => {
            let id = node.id();
            document
                .node_mut(id)
                .map_err(invalid_edit)?
                .set_scalar(value)
                .map_err(invalid_edit)?;
            document.to_string()
        }
    };
    let check = parse(edited.as_str().to_owned()).map_err(EditError::Blocked)?;
    if !matches!(target(&check, key), Ok(Target::Replace(_, actual)) if actual == value) {
        return Err(EditError::Blocked(INVALID_EDIT));
    }
    Ok(edited)
}

fn parse(source: String) -> Result<Document, &'static str> {
    let settings = Settings::new()
        .merge_keys(true)
        .honor_version_directive(true);
    Document::parse_with(source, &settings).map_err(|error| match error.code() {
        code::DUPLICATE_KEY => "ambiguous YAML document: duplicate key",
        code::MULTIPLE_DOCUMENTS => "ambiguous YAML file: expected a single document",
        _ => "invalid YAML document",
    })
}

fn target<'a>(document: &'a Document, key: &[String]) -> Result<Target<'a>, &'static str> {
    let Some(mut node) = document.root().filter(|root| !root.span().is_empty()) else {
        return Ok(Target::MissingParent(0));
    };
    for (index, name) in key.iter().enumerate() {
        check_aliases(document, node)?;
        let object = node
            .as_mapping()
            .ok_or("expected a YAML object on the key path")?;
        let last = index + 1 == key.len();
        let Some(value) = object.get(name.as_str()) else {
            return Ok(if last {
                Target::AddTo(node.id())
            } else {
                Target::MissingParent(index + 1)
            });
        };
        // Lookup can see merged values. Only edit entries written in this object.
        let Some((entry_key, _)) = object.iter().find(|(_, own)| own.id() == value.id()) else {
            return Err("ambiguous YAML key path: key comes from a merge");
        };
        if entry_key.kind() == NodeKind::Alias {
            return Err("ambiguous YAML key path: key is an alias");
        }
        node = value;
        if last {
            check_aliases(document, node)?;
            let value = node
                .as_str()
                .ok_or("expected a YAML string at the selected field")?;
            return Ok(Target::Replace(node, value));
        }
    }
    unreachable!("config requires a non-empty key path")
}

fn replace_block(
    document: &Document,
    field: NodeRef<'_>,
    value: &str,
) -> Result<String, &'static str> {
    // The block scalar edit API can reject header comments. Edit only the two tokens.
    let scalar = document
        .syntax()
        .root()
        .descendants()
        .find(|node| node.span() == field.span() && node.block_header().is_some())
        .ok_or(INVALID_EDIT)?;
    let header = scalar.block_header().ok_or(INVALID_EDIT)?.span;
    let body = scalar.last_child().ok_or(INVALID_EDIT)?.span();

    let mut quoted = Document::parse("\"\"").map_err(|_| INVALID_EDIT)?;
    quoted
        .root_mut()
        .and_then(|mut node| node.set_scalar(value).map(|_| ()))
        .map_err(|_| INVALID_EDIT)?;
    let quoted = quoted.root().ok_or(INVALID_EDIT)?.text();
    let mut edited = document.to_string();
    edited.replace_range(body.start..body.end, "");
    edited.replace_range(header.start..header.end, &quoted);
    Ok(edited)
}

fn check_aliases(document: &Document, node: NodeRef<'_>) -> Result<(), &'static str> {
    if node.kind() == NodeKind::Alias {
        return Err("ambiguous YAML key path: value is an alias");
    }
    if node.anchor().is_some()
        && !document
            .aliases_of(node.id())
            .map_err(|_| INVALID_EDIT)?
            .is_empty()
    {
        return Err("ambiguous YAML key path: value is used by an alias");
    }
    Ok(())
}
