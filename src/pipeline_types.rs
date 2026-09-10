//! C32: fictional typed data for component inputs.
//!
//! Parses `name = input.required<T>()` / `input<T>(d)` / `model<T>(d)`
//! declarations, resolves referenced types against the project sources
//! (nearest-first, bounded), and generates deterministic fictional JSON
//! values by field name + type so dynamically mounted components render
//! with plausible data.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

/// A component input declaration parsed from source.
#[derive(Debug, Clone)]
pub struct ParsedInput {
    pub name: String,
    #[allow(dead_code)] // semantic flag; generation covers required via generic
    pub required: bool,
    pub generic: Option<String>,
    pub has_default: bool,
}

/// Parse `name = input.required<T>()`, `name = input<T>(d)` and
/// `name = model<T>(d)` declarations from the component source.
pub fn parse_component_inputs(source: &str) -> Vec<ParsedInput> {
    let mut out: Vec<ParsedInput> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = source[from..].find('=') {
        let eq = from + rel;
        let before = source[..eq].trim_end();
        let name: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        let name: String = name.chars().rev().collect();
        let after = eq + 1;
        let rest = source[after..].trim_start();
        let token = if rest.starts_with("input.required") {
            Some(("input.required", true))
        } else if rest.starts_with("input") {
            Some(("input", false))
        } else if rest.starts_with("model") {
            Some(("model", false))
        } else {
            None
        };
        if !name.is_empty() {
            if let Some((token, required)) = token {
                let raw = &source[after..];
                let leading_ws = raw.len() - raw.trim_start().len();
                let mut pos = after + leading_ws + token.len();
                let mut generic: Option<String> = None;
                let all = source.as_bytes();
                if all.get(pos) == Some(&b'<') {
                    if let Some((inner, close)) = balanced_block(source, pos, b'<', b'>') {
                        generic = Some(inner);
                        pos = close + 1;
                    }
                }
                let mut has_default = false;
                if all.get(pos) == Some(&b'(') {
                    if let Some((inner, _)) = balanced_block(source, pos, b'(', b')') {
                        has_default = !inner.trim().is_empty();
                    }
                }
                if !out.iter().any(|i: &ParsedInput| i.name == name) {
                    out.push(ParsedInput {
                        name,
                        required,
                        generic,
                        has_default,
                    });
                }
            }
        }
        from = eq + 1;
    }
    out
}

/// Balanced block scan starting at `open`. Returns (inner, close_index).
fn balanced_block(source: &str, open: usize, open_b: u8, close_b: u8) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&open_b) {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            c if c == open_b => depth += 1,
            c if c == close_b => {
                depth -= 1;
                if depth == 0 {
                    return Some((source[open + 1..i].to_string(), i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone)]
struct TypeDefBody {
    body: String,
    is_enum: bool,
}

/// Bounded registry of interface/type/enum/class definitions discovered in
/// the project, resolved nearest-first from the component.
#[derive(Default)]
pub struct TypeIndex {
    root: PathBuf,
    component: PathBuf,
    defs: HashMap<String, TypeDefBody>,
    missing: HashSet<String>,
}

impl TypeIndex {
    pub fn new(root: PathBuf, component: PathBuf) -> Self {
        TypeIndex {
            root,
            component,
            defs: HashMap::new(),
            missing: HashSet::new(),
        }
    }

    fn resolve(&mut self, name: &str) -> Option<TypeDefBody> {
        if let Some(def) = self.defs.get(name) {
            return Some(def.clone());
        }
        if self.missing.contains(name) {
            return None;
        }
        self.search_fs(name);
        let found = self.defs.get(name).cloned();
        if found.is_none() {
            self.missing.insert(name.to_string());
        }
        found
    }

    fn search_fs(&mut self, name: &str) {
        let mut reads = 0usize;
        let mut dir = self.component.parent().map(|p| p.to_path_buf());
        loop {
            let Some(current) = dir else { break };
            if let Ok(entries) = fs::read_dir(&current) {
                let mut files: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension().and_then(|e| e.to_str()) == Some("ts")
                            && !p.to_string_lossy().contains(".spec.")
                    })
                    .collect();
                files.sort();
                for file in files.into_iter().take(40) {
                    if reads >= 200 {
                        return;
                    }
                    reads += 1;
                    if let Ok(content) = fs::read_to_string(&file) {
                        if let Some(def) = extract_type_def(&content, name) {
                            self.defs.insert(name.to_string(), def);
                            return;
                        }
                    }
                }
            }
            if current == self.root {
                break;
            }
            dir = current.parent().map(|p| p.to_path_buf());
        }
    }
}

/// Extract `interface NAME {...}` / `type NAME = ...;` / `enum NAME {...}` /
/// `class NAME {...}` bodies from a source file.
fn extract_type_def(content: &str, name: &str) -> Option<TypeDefBody> {
    for keyword in ["interface", "enum", "class", "type"] {
        let needle = format!("{keyword} {name}");
        let mut from = 0usize;
        loop {
            let Some(pos) = content[from..].find(&needle) else {
                break;
            };
            let abs = from + pos;
            let after = &content[abs + needle.len()..];
            let boundary_ok = after
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '{' || c == '=' || c == '<');
            if !boundary_ok {
                from = abs + needle.len();
                continue;
            }
            if keyword == "type" {
                let Some(rhs_idx) = content[abs..].find('=') else {
                    break;
                };
                let rhs_idx = abs + rhs_idx + 1;
                let rhs = &content[rhs_idx..];
                let trimmed = rhs.trim_start();
                if trimmed.starts_with('{') {
                    let brace = rhs_idx + (rhs.len() - trimmed.len());
                    let (body, _) = balanced_block(content, brace, b'{', b'}')?;
                    return Some(TypeDefBody {
                        body,
                        is_enum: false,
                    });
                }
                let end = rhs.find(';').unwrap_or(rhs.len());
                return Some(TypeDefBody {
                    body: rhs[..end].trim().to_string(),
                    is_enum: false,
                });
            }
            // interface/enum/class: capture the first balanced brace block.
            let Some(brace_rel) = content[abs..].find('{') else {
                break;
            };
            let brace = abs + brace_rel;
            let (body, _) = balanced_block(content, brace, b'{', b'}')?;
            return Some(TypeDefBody {
                body,
                is_enum: keyword == "enum",
            });
        }
    }
    None
}

/// Top-level `name: type` fields of an interface/class/type-object body.
fn interface_fields(body: &str) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut part = String::new();
    let mut depth = 0i32;
    let mut in_string: Option<u8> = None;
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_string {
            part.push(c as char);
            if c == b'\\' {
                i += 1;
                if i < bytes.len() {
                    part.push(bytes[i] as char);
                }
            } else if c as u8 == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => {
                in_string = Some(c);
                part.push(c as char);
            }
            b'(' | b'{' | b'[' => {
                depth += 1;
                part.push(c as char);
            }
            b')' | b'}' | b']' => {
                depth -= 1;
                part.push(c as char);
            }
            b';' | b'\n' if depth == 0 => {
                push_field(&part, &mut fields);
                part.clear();
            }
            _ => part.push(c as char),
        }
        i += 1;
    }
    push_field(&part, &mut fields);
    fields
}

fn push_field(part: &str, fields: &mut Vec<(String, String)>) {
    let trimmed = part.trim();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
        return;
    }
    let cleaned_line = cleaned_line(trimmed);
    let cleaned = cleaned_line
        .strip_suffix(';')
        .unwrap_or(&cleaned_line)
        .trim();
    let cleaned = cleaned.strip_prefix("readonly ").unwrap_or(cleaned).trim();
    let Some(idx) = cleaned.find(':') else { return };
    let name = cleaned[..idx].trim().trim_end_matches('?').trim();
    let name: String = name
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    let mut ty = cleaned[idx + 1..].trim();
    if let Some(eq) = ty.find('=') {
        ty = ty[..eq].trim();
    }
    if !name.is_empty() && !ty.is_empty() {
        fields.push((name.to_string(), ty.to_string()));
    }
}

fn cleaned_line(trimmed: &str) -> String {
    trimmed
        .split('\n')
        .map(strip_line_comment)
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_line_comment(line: &str) -> String {
    let l = line.trim_start();
    if l.starts_with("//") {
        return String::new();
    }
    match l.find("//") {
        Some(idx) if idx > 0 && !l[..idx].ends_with(':') => l[..idx].to_string(),
        _ => line.to_string(),
    }
}

/// First non-null/undefined alternative of a top-level union.
fn first_union_alternative(ty: &str) -> String {
    let mut depth = 0i32;
    let bytes = ty.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut alts: Vec<String> = Vec::new();
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_string {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c as u8 == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => in_string = Some(c),
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' | b')' | b']' | b'}' => depth -= 1,
            b'|' if depth == 0 => {
                alts.push(ty[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    alts.push(ty[start..].trim().to_string());
    alts.into_iter()
        .find(|a| !a.is_empty() && a != "null" && a != "undefined")
        .unwrap_or_else(|| ty.trim().to_string())
}

fn fake_string(field: &str) -> String {
    let n = field.to_lowercase();
    if n.contains("id") {
        "id-1024".to_string()
    } else if n.contains("email") || n.contains("mail") {
        "user@example.com".to_string()
    } else if n.contains("url") || n.contains("link") || n.contains("href") {
        "https://example.com".to_string()
    } else if n.contains("date") || n.contains("fecha") || n.ends_with("at") {
        "2026-01-15".to_string()
    } else if n.contains("name") || n.contains("title") || n.contains("label") {
        "Sample name".to_string()
    } else if n.contains("desc") {
        "Sample description".to_string()
    } else if n.contains("img") || n.contains("photo") || n.contains("icon") || n.contains("avatar")
    {
        "https://example.com/image.png".to_string()
    } else if n.contains("color") {
        "#3178C6".to_string()
    } else {
        "Sample text".to_string()
    }
}

fn fake_number(field: &str) -> serde_json::Number {
    let n = field.to_lowercase();
    if n.contains("total") || n.contains("count") || n.contains("cantidad") || n.contains("size") {
        serde_json::Number::from(10)
    } else {
        serde_json::Number::from(1)
    }
}

/// Generate a fictional JSON value for a TypeScript type (C32).
pub fn fake_value(ty: &str, field_hint: &str, index: &mut TypeIndex, depth: usize) -> Value {
    let ty = ty.trim().trim_end_matches(',').trim();
    if depth > 4 {
        return Value::Null;
    }
    if cleaned_line(ty).trim().is_empty() {
        return Value::Null;
    }

    // Array: T[] or Array<T>
    if let Some(inner) = ty.strip_suffix("[]") {
        let item = fake_value(inner, field_hint, index, depth + 1);
        return Value::Array(vec![item]);
    }
    if let Some(inner) = ty.strip_prefix("Array<").and_then(|s| s.strip_suffix('>')) {
        let item = fake_value(inner, field_hint, index, depth + 1);
        return Value::Array(vec![item]);
    }

    // Union: first usable alternative (string literals handled after).
    if !ty.starts_with('\'') && !ty.starts_with('"') && ty.contains('|') {
        let alt = first_union_alternative(ty);
        if alt != *ty {
            return fake_value(&alt, field_hint, index, depth);
        }
    }

    // String literal type
    if (ty.starts_with('\'') && ty.ends_with('\'') && ty.len() >= 2)
        || (ty.starts_with('"') && ty.ends_with('"') && ty.len() >= 2)
    {
        return Value::String(ty[1..ty.len() - 1].to_string());
    }

    match ty {
        "string" => return Value::String(fake_string(field_hint)),
        "number" => return Value::Number(fake_number(field_hint)),
        "boolean" | "bool" => return Value::Bool(true),
        "Date" => return Value::String("2026-01-15T10:00:00Z".to_string()),
        "any" | "unknown" | "object" => return serde_json::json!({}),
        _ => {}
    }
    if ty.starts_with("Record<") || ty.starts_with("Map<") || ty.starts_with("Set<") {
        return serde_json::json!({});
    }

    // Named type: resolve through the index.
    let bare = ty.trim_start_matches("readonly ").trim().to_string();
    let base_name: String = bare
        .split(['<', ' ', '|'])
        .next()
        .unwrap_or_default()
        .to_string();
    if base_name.is_empty() {
        return serde_json::json!({});
    }
    if let Some(def) = index.resolve(&base_name) {
        if def.is_enum {
            for part in def.body.split(',') {
                let part = part.trim();
                if let Some(idx_eq) = part.find('=') {
                    let value = part[idx_eq + 1..].trim();
                    let value = value.trim_matches('\'').trim_matches('"');
                    if !value.is_empty()
                        && value
                            .chars()
                            .next()
                            .map(|c| !c.is_numeric())
                            .unwrap_or(false)
                    {
                        return Value::String(value.to_string());
                    }
                }
            }
            return Value::Number(serde_json::Number::from(0));
        }
        let mut obj = serde_json::Map::new();
        for (field_name, field_type) in interface_fields(&def.body) {
            obj.insert(
                field_name.clone(),
                fake_value(&field_type, &field_name, index, depth + 1),
            );
        }
        return Value::Object(obj);
    }

    // Generic interfaces (Page<T> etc.) and unresolved names: empty object.
    serde_json::json!({})
}
