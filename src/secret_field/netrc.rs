use std::ops::Range;

use super::EditError;

const KEY_ERROR: &str = r#"netrc key must be ["hostname", "password"]"#;
const SYNTAX_ERROR: &str = "invalid or unsupported .netrc syntax";
const KEYWORDS: [&str; 6] = [
    "machine", "default", "login", "password", "account", "macdef",
];

pub(super) fn validate_key(key: &[String]) -> Result<(), &'static str> {
    match key {
        [host, field] if bare_value(host) && field == "password" => Ok(()),
        _ => Err(KEY_ERROR),
    }
}

pub(super) fn is_set(source: &str, host: &str) -> Result<bool, &'static str> {
    Ok(find(source, host)?
        .and_then(|machine| machine.password)
        .is_some_and(|(_, value)| &source[value] != "\"\""))
}

pub(super) fn edit(
    mut source: String,
    host: &str,
    value: &str,
) -> Result<Option<String>, EditError> {
    if !value.is_empty() && !bare_value(value) {
        return Err(EditError::Blocked(
            "netrc passwords must be single unquoted tokens without whitespace, quotes, backslashes, or control characters",
        ));
    }
    let Some(machine) = find(&source, host).map_err(EditError::Blocked)? else {
        return if value.is_empty() {
            Ok(None)
        } else {
            Err(EditError::Blocked("selected .netrc machine does not exist"))
        };
    };
    match (machine.password, value.is_empty()) {
        (None, true) => return Ok(None),
        (None, false) => source.insert_str(machine.end, &format!(" password {value}")),
        (Some((_, old)), false) => source.replace_range(old, value),
        (Some((keyword, old)), true) => remove_password(&mut source, keyword, old),
    }
    Ok(Some(source))
}

fn bare_value(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('#') && token_text(value) && keyword(value).is_none()
}

fn token_text(value: &str) -> bool {
    !value
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '"' | '\\'))
}

fn keyword(token: &str) -> Option<&'static str> {
    KEYWORDS
        .iter()
        .copied()
        .find(|word| word.eq_ignore_ascii_case(token))
}

#[derive(Default)]
struct Machine {
    end: usize,
    login: bool,
    account: bool,
    password: Option<(Range<usize>, Range<usize>)>,
}

fn find(source: &str, host: &str) -> Result<Option<Machine>, &'static str> {
    if source.contains('\0') {
        return Err(SYNTAX_ERROR);
    }
    let mut tokens = Tokens { source, offset: 0 };
    let mut next = tokens.next()?;
    let mut selected = None;
    while let Some(start) = next {
        let (name, end) = match keyword(&source[start.clone()]) {
            Some("machine") => {
                let name = tokens.value()?;
                let text = &source[name.clone()];
                let text = text
                    .strip_prefix('"')
                    .and_then(|text| text.strip_suffix('"'))
                    .unwrap_or(text);
                if !bare_value(text) {
                    return Err("unsupported .netrc machine name");
                }
                (Some(text), name.end)
            }
            Some("default") => (None, start.end),
            Some("macdef") => return Err(".netrc macros are not supported"),
            _ => return Err(SYNTAX_ERROR),
        };
        let mut machine = Machine {
            end,
            ..Machine::default()
        };
        loop {
            next = tokens.next()?;
            let Some(field) = next.clone() else { break };
            let kind = match keyword(&source[field.clone()]) {
                Some("machine" | "default" | "macdef") if name.is_some() => break,
                Some(kind @ ("login" | "account" | "password")) => kind,
                _ => return Err(SYNTAX_ERROR),
            };
            let value = tokens.value()?;
            machine.end = value.end;
            match kind {
                "login" if !machine.login => machine.login = true,
                "account" if !machine.account => machine.account = true,
                "password" if machine.password.is_none() => {
                    machine.password = Some((field, value));
                }
                _ => return Err("duplicate .netrc field"),
            }
        }
        if name.is_some_and(|name| name.eq_ignore_ascii_case(host))
            && selected.replace(machine).is_some()
        {
            return Err("duplicate .netrc machine");
        }
    }
    Ok(selected)
}

struct Tokens<'a> {
    source: &'a str,
    offset: usize,
}

impl Tokens<'_> {
    fn next(&mut self) -> Result<Option<Range<usize>>, &'static str> {
        loop {
            let rest = &self.source[self.offset..];
            let token = rest.trim_start_matches([' ', '\t', '\r', '\n']);
            self.offset += rest.len() - token.len();
            if token.is_empty() {
                return Ok(None);
            }
            if !token.starts_with('#') {
                break;
            }
            let line = self.source[..self.offset].rsplit('\n').next().unwrap();
            if !line.trim_matches([' ', '\t']).is_empty() {
                return Err("only full-line .netrc comments are supported");
            }
            self.offset += token.find('\n').unwrap_or(token.len());
        }
        let start = self.offset;
        let token = &self.source[start..];
        let len = if let Some(quoted) = token.strip_prefix('"') {
            let mut escaped = false;
            let mut end = None;
            for (offset, ch) in quoted.char_indices() {
                if ch.is_control() {
                    return Err(SYNTAX_ERROR);
                }
                if ch == '"' && !escaped {
                    end = Some(offset + 2);
                    break;
                }
                escaped = ch == '\\' && !escaped;
            }
            let end = end.ok_or(SYNTAX_ERROR)?;
            if !token[end..].is_empty() && !token[end..].starts_with([' ', '\t', '\r', '\n']) {
                return Err(SYNTAX_ERROR);
            }
            end
        } else {
            let end = token.find([' ', '\t', '\r', '\n']).unwrap_or(token.len());
            let text = &token[..end];
            if !token_text(text) {
                return Err(SYNTAX_ERROR);
            }
            end
        };
        self.offset += len;
        Ok(Some(start..self.offset))
    }

    fn value(&mut self) -> Result<Range<usize>, &'static str> {
        self.next()?
            .filter(|span| keyword(&self.source[span.clone()]).is_none())
            .ok_or("missing .netrc field value")
    }
}

fn remove_password(source: &mut String, keyword: Range<usize>, value: Range<usize>) {
    if source[keyword.end..value.start].contains(['\r', '\n']) {
        source.replace_range(value, "");
        source.replace_range(keyword, "");
        return;
    }

    let before = &source[..keyword.start];
    let after = &source[value.end..];
    let start = before.trim_end_matches([' ', '\t']).len();
    let end = after.find('\n').unwrap_or(after.len());
    if (start == 0 || before[..start].ends_with('\n'))
        && after[..end].trim_matches([' ', '\t', '\r']).is_empty()
    {
        source.replace_range(start..value.end + end + usize::from(end < after.len()), "");
    } else {
        source.replace_range(start..value.end, "");
    }
}
