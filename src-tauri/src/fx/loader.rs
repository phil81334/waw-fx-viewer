//! Preview-only EFX text reader. No asset registration, audio or game process.
//!
//! The grammar follows the pinned iw3xo `fx_load.cpp` reference. We deliberately
//! preserve tokens and declaration order rather than claiming to compile every
//! EffectsEd feature here. The simulation owns native flag/range conversion.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug)]
struct Token {
    text: String,
    quoted: bool,
    line: usize,
}

/// Tokenise punctuation and comments without mistaking quoted paths for syntax.
fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.trim_start_matches('\u{feff}').chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1;

    while index < chars.len() {
        let current = chars[index];
        if current.is_whitespace() {
            if current == '\n' {
                line += 1;
            }
            index += 1;
            continue;
        }

        if current == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }

        if current == '/' && chars.get(index + 1) == Some(&'*') {
            let start_line = line;
            index += 2;
            let mut closed = false;
            while index < chars.len() {
                if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                    index += 2;
                    closed = true;
                    break;
                }
                if chars[index] == '\n' {
                    line += 1;
                }
                index += 1;
            }
            if !closed {
                return Err(format!("Unterminated block comment at line {start_line}"));
            }
            continue;
        }

        let start_line = line;
        if current == '"' {
            index += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < chars.len() {
                let ch = chars[index];
                if ch == '"' {
                    index += 1;
                    closed = true;
                    break;
                }
                // Only quote/backslash escapes are decoded. Unknown sequences
                // remain literal, so a path such as textures\new is not damaged.
                if ch == '\\' {
                    if let Some(next) = chars.get(index + 1) {
                        if *next == '"' || *next == '\\' {
                            value.push(*next);
                            index += 2;
                            continue;
                        }
                    }
                }
                if ch == '\n' {
                    line += 1;
                }
                value.push(ch);
                index += 1;
            }
            if !closed {
                return Err(format!("Unterminated quoted string at line {start_line}"));
            }
            tokens.push(Token {
                text: value,
                quoted: true,
                line: start_line,
            });
            continue;
        }

        if matches!(current, '{' | '}' | ';') {
            tokens.push(Token {
                text: current.to_string(),
                quoted: false,
                line,
            });
            index += 1;
            continue;
        }

        let start = index;
        while index < chars.len() {
            let ch = chars[index];
            if ch.is_whitespace() || matches!(ch, '{' | '}' | ';' | '"') {
                break;
            }
            if ch == '/' && matches!(chars.get(index + 1), Some('/' | '*')) {
                break;
            }
            index += 1;
        }
        tokens.push(Token {
            text: chars[start..index].iter().collect(),
            quoted: false,
            line: start_line,
        });
    }
    Ok(tokens)
}

fn punctuation(token: &Token, expected: &str) -> bool {
    !token.quoted && token.text == expected
}

fn parse_text(text: &str, name: &str, hash: &str) -> Result<Value, String> {
    let tokens = tokenize(text)?;
    if tokens.len() < 2 || !tokens[0].text.eq_ignore_ascii_case("iwfx") {
        return Err("Expected iwfx header and version".to_string());
    }
    let version: u32 = tokens[1].text.parse().map_err(|_| "Invalid iwfx version")?;
    if version != 1 && version != 2 {
        return Err(format!("Unsupported iwfx version {version}"));
    }

    let mut elements = Vec::new();
    let mut declarations = Vec::new();
    let mut duplicate_fields = Vec::new();
    let mut priority = 0;
    let mut priority_seen = false;
    let mut index = 2;

    while index < tokens.len() {
        if !punctuation(&tokens[index], "{") {
            return Err(format!(
                "Expected element opening brace at line {}",
                tokens[index].line
            ));
        }
        if elements.len() == 32 {
            return Err("More than 32 source elements".to_string());
        }
        index += 1;
        let mut element = Map::new();
        let mut ordered = Vec::new();
        let mut closed = false;

        while index < tokens.len() {
            if punctuation(&tokens[index], "}") {
                index += 1;
                closed = true;
                break;
            }
            let field = &tokens[index];
            if field.quoted || matches!(field.text.as_str(), "{" | ";") {
                return Err(format!("Expected field name at line {}", field.line));
            }
            let key = field.text.clone();
            let field_line = field.line;
            index += 1;
            let mut values = Vec::new();
            let mut depth = 0_usize;
            let mut terminated = false;

            while index < tokens.len() {
                let token = &tokens[index];
                index += 1;
                if punctuation(token, ";") && depth == 0 {
                    terminated = true;
                    break;
                }
                if punctuation(token, "{") {
                    depth += 1;
                } else if punctuation(token, "}") {
                    if depth == 0 {
                        return Err(format!(
                            "Missing semicolon after {key} at line {field_line}"
                        ));
                    }
                    depth -= 1;
                }
                values.push(token.text.clone());
            }
            if !terminated {
                return Err(format!("Unterminated field {key} at line {field_line}"));
            }
            ordered.push(json!({"field": key, "tokens": values, "line": field_line}));
            if element.contains_key(&key) {
                duplicate_fields
                    .push(json!({"element": elements.len(), "field": key, "line": field_line}));
            }
            if key == "efPriority" && !priority_seen {
                priority = values
                    .first()
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or_else(|| format!("Invalid efPriority at line {field_line}"))?;
                priority_seen = true;
            }
            element.insert(key, json!(values));
        }
        if !closed {
            return Err("Unterminated element".to_string());
        }

        // These named defaults were executed in the pinned WaW initializer.
        // The unnamed +0x100 float is recorded below, never assigned a guess.
        for (field, value) in [
            ("sortOrder", "5"),
            ("lightingFrac", "0"),
            ("windinfluence", "0"),
        ] {
            if !element.contains_key(field) {
                element.insert(field.to_string(), json!([value]));
            }
        }
        elements.push(Value::Object(element));
        declarations.push(ordered);
    }

    Ok(json!({
        "name": name,
        "version": version,
        "sha256": hash,
        "elements": elements,
        "declarations": declarations,
        "duplicateFields": duplicate_fields,
        "efPriority": priority,
        "nativeDefaults": {
            "sortOrder": 5,
            "lightingFrac": 0,
            "windinfluence": 0,
            "useBackCompatVel": version == 1,
            "unnamedEditorFloat0x100": 1.0
        }
    }))
}

/// Read an actual loose EFX file; names are relative to raw/fx where possible.
pub fn load(path: &Path) -> Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let normalised = path.to_string_lossy().replace('\\', "/");
    let lower = normalised.to_ascii_lowercase();
    let name = if let Some(start) = lower.rfind("/raw/fx/") {
        normalised[start + 8..].trim_end_matches(".efx").to_string()
    } else {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    parse_text(text, &name, &hash).map_err(|error| format!("{}: {error}", path.display()))
}

/// Return decoded token strings without applying field-specific semantics.
pub fn words(element: &Value, key: &str) -> Vec<String> {
    element
        .get(key)
        .and_then(Value::as_array)
        .map(|tokens| {
            tokens
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Convenience for already-validated base/amplitude ranges in the simulation.
pub fn pair(element: &Value, key: &str, default: [f64; 2]) -> [f64; 2] {
    let tokens = words(element, key);
    if tokens.len() != 2 {
        return default;
    }
    match (tokens[0].parse::<f64>(), tokens[1].parse::<f64>()) {
        (Ok(first), Ok(second)) if first.is_finite() && second.is_finite() => [first, second],
        _ => default,
    }
}

/// Read the two native piecewise-linear curves. Values remain unscaled here.
pub fn graph(element: &Value, key: &str, dimensions: usize) -> Result<Value, String> {
    if dimensions == 0 || dimensions > 3 {
        return Err(format!(
            "Unsupported graph dimensions for {key}: {dimensions}"
        ));
    }
    let tokens = words(element, key);
    let number = |word: &str| -> Result<f64, String> {
        let value = word
            .parse::<f64>()
            .map_err(|_| format!("Invalid number in {key}: {word}"))?;
        if !value.is_finite() {
            return Err(format!("Non-finite number in {key}"));
        }
        Ok(value)
    };
    let scale = number(
        tokens
            .first()
            .ok_or_else(|| format!("Missing graph {key}"))?,
    )?;
    let mut index = 1;
    let expect = |index: &mut usize, expected: &str| -> Result<(), String> {
        if tokens.get(*index).map(String::as_str) != Some(expected) {
            return Err(format!("Expected {expected} in {key} at token {index}"));
        }
        *index += 1;
        Ok(())
    };
    expect(&mut index, "{")?;
    let mut curves = Vec::new();
    for _ in 0..2 {
        expect(&mut index, "{")?;
        let mut rows: Vec<Vec<f64>> = Vec::new();
        while tokens.get(index).map(String::as_str) != Some("}") {
            let mut row = Vec::new();
            for _ in 0..=dimensions {
                let token = tokens
                    .get(index)
                    .ok_or_else(|| format!("Truncated graph {key}"))?;
                row.push(number(token)?);
                index += 1;
            }
            if let Some(previous) = rows.last() {
                if row[0] <= previous[0] {
                    return Err(format!("Non-increasing graph times in {key}"));
                }
            }
            rows.push(row);
            if rows.len() * (dimensions + 1) > 512 {
                return Err(format!("Too many curve values in {key}"));
            }
        }
        expect(&mut index, "}")?;
        if rows.len() < 2 || rows.first().unwrap()[0] != 0.0 || rows.last().unwrap()[0] != 1.0 {
            return Err(format!("Graph {key} needs endpoints at 0 and 1"));
        }
        curves.push(rows);
    }
    expect(&mut index, "}")?;
    if index != tokens.len() {
        return Err(format!("Trailing tokens in graph {key}"));
    }
    Ok(json!({"scale": scale, "curves": curves}))
}
