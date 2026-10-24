//! A deliberately bounded reader for installed text techset label groups.
//! This selects named techniques only; it does not relax shader or state guards.
use std::collections::BTreeMap;

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Label(&'a str),
    Technique(&'a str),
    Colon,
    Semicolon,
}

fn tokenize(text: &str) -> Result<Vec<Token<'_>>, String> {
    let bytes = text.as_bytes();
    let mut position = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let mut tokens = Vec::new();

    while position < bytes.len() {
        if bytes[position].is_ascii_whitespace() {
            position += 1;
            continue;
        }

        if bytes[position..].starts_with(b"//") {
            while position < bytes.len() && bytes[position] != b'\n' {
                position += 1;
            }
            continue;
        }

        if bytes[position..].starts_with(b"/*") {
            let tail = &text[position + 2..];
            let end = tail.find("*/").ok_or("Unterminated techset comment")?;
            position += end + 4;
            continue;
        }

        match bytes[position] {
            b':' => {
                tokens.push(Token::Colon);
                position += 1;
            }
            b';' => {
                tokens.push(Token::Semicolon);
                position += 1;
            }
            b'"' => {
                position += 1;
                let begin = position;
                while position < bytes.len() && bytes[position] != b'"' {
                    if !bytes[position].is_ascii_graphic() && bytes[position] != b' ' {
                        return Err("Invalid character in techset label".into());
                    }
                    if bytes[position] == b'\\' {
                        return Err("Escaped techset labels are outside the checked grammar".into());
                    }
                    position += 1;
                }
                if position == bytes.len() || begin == position {
                    return Err("Empty or unterminated techset label".into());
                }
                tokens.push(Token::Label(&text[begin..position]));
                position += 1;
            }
            byte if byte.is_ascii_alphanumeric() || byte == b'_' => {
                let begin = position;
                while position < bytes.len()
                    && (bytes[position].is_ascii_alphanumeric() || bytes[position] == b'_')
                {
                    position += 1;
                }
                tokens.push(Token::Technique(&text[begin..position]));
            }
            _ => {
                return Err(format!("Unsupported techset token at byte {position}"));
            }
        }
    }
    Ok(tokens)
}

/// Return the technique assigned to the requested label.
/// All input is checked before selecting a label, so trailing corruption cannot
/// accidentally turn into an admitted graphics path. Repeated labels inside
/// one group are idempotent (38 installed files contain these). Assigning the
/// same label to different techniques rejects instead of guessing precedence.
pub fn technique(text: &str, requested: &str) -> Result<Option<String>, String> {
    let tokens = tokenize(text)?;
    let mut mapping = BTreeMap::new();
    let mut position = 0;

    while position < tokens.len() {
        let mut labels = Vec::new();
        while let Some(Token::Label(label)) = tokens.get(position) {
            if tokens.get(position + 1) != Some(&Token::Colon) {
                return Err(format!("Missing colon after techset label {label}"));
            }
            labels.push(*label);
            position += 2;
        }
        if labels.is_empty() {
            return Err("Technique has no techset label".into());
        }
        let Some(Token::Technique(name)) = tokens.get(position) else {
            return Err("Techset label group has no technique".into());
        };
        if tokens.get(position + 1) != Some(&Token::Semicolon) {
            return Err(format!("Missing semicolon after techset technique {name}"));
        }
        for label in labels {
            if mapping.get(label).is_some_and(|previous| previous != name) {
                return Err(format!("Duplicate techset label {label}"));
            }
            mapping.insert(label, *name);
        }
        position += 2;
    }

    Ok(mapping.get(requested).map(|name| (*name).to_owned()))
}
