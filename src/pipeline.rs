//! Pre-serve validation of Angular component sources (C10, C12).
//! Errors must surface early (explorer status / CLI exit), never as a crash at render time.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use crate::detect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    Standalone,
    Module,
}

impl Strategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Standalone => "standalone",
            Strategy::Module => "module",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRef {
    Inline,
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StyleRef {
    Inline,
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct ComponentInfo {
    pub class_name: String,
    /// Names of `input.required<T>()` signal inputs found in the source — the
    /// host must provide placeholder values at mount or Angular throws NG0950.
    pub required_inputs: Vec<String>,
    /// Fictional typed values for the component inputs (C32), keyed by input name.
    pub input_values: serde_json::Map<String, Value>,
    /// Present when a sibling `.module.ts` declares this component (D3).
    pub module_class_name: Option<String>,
    #[allow(dead_code)] // metadata kept for consumers (tests assert it)
    pub template: TemplateRef,
    #[allow(dead_code)]
    pub styles: Vec<StyleRef>,
    pub strategy: Strategy,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PipelineError {
    #[error("not an Angular component: no @Component decorator found")]
    NotAComponent,
    #[error("unparseable component: {0}")]
    Parse(String),
    #[error("template file not found: {0}")]
    TemplateMissing(PathBuf),
    #[error("style file not found: {0}")]
    StyleMissing(PathBuf),
}

/// Validate the component source and resolve its metadata before anything is
/// served. `root` bounds the search scope for type resolution (C32).
pub fn validate(component: &Path, root: &Path) -> Result<ComponentInfo, PipelineError> {
    let source = fs::read_to_string(component)
        .map_err(|e| PipelineError::Parse(format!("cannot read {}: {e}", component.display())))?;

    if !source.contains("@Component") {
        return Err(PipelineError::NotAComponent);
    }
    let decorator = extract_decorator_block(&source)
        .ok_or_else(|| PipelineError::Parse("unbalanced @Component(...) block".into()))?;
    let block = &source[decorator.clone()];

    let template = match extract_string_value(block, "templateUrl") {
        Some(rel) => {
            let file = component.parent().unwrap_or(Path::new(".")).join(&rel);
            if !file.is_file() {
                return Err(PipelineError::TemplateMissing(file));
            }
            TemplateRef::File(file)
        }
        None => {
            if extract_string_value(block, "template").is_some() {
                TemplateRef::Inline
            } else {
                return Err(PipelineError::Parse(
                    "no template or templateUrl in @Component".into(),
                ));
            }
        }
    };

    let mut styles = Vec::new();
    for rel in extract_string_array(block, "styleUrls") {
        let file = component.parent().unwrap_or(Path::new(".")).join(&rel);
        if !file.is_file() {
            return Err(PipelineError::StyleMissing(file));
        }
        styles.push(StyleRef::File(file));
    }
    if styles.is_empty() && !extract_string_array(block, "styles").is_empty() {
        styles.push(StyleRef::Inline);
    }

    let class_name = class_name_after(&source, decorator.end)
        .ok_or_else(|| PipelineError::Parse("no class found after @Component".into()))?;

    let (module_class_name, strategy) = match detect::sibling_module(component) {
        Some(module_path) => {
            let module_source = fs::read_to_string(&module_path).map_err(|e| {
                PipelineError::Parse(format!("cannot read module {}: {e}", module_path.display()))
            })?;
            let module_class = module_class_name(&module_source).ok_or_else(|| {
                PipelineError::Parse(format!(
                    "no exported class in module {}",
                    module_path.display()
                ))
            })?;
            (Some(module_class), Strategy::Module)
        }
        None => (None, Strategy::Standalone),
    };

    // C32: auto-fill inputs with fictional typed data.
    let parsed_inputs = crate::pipeline_types::parse_component_inputs(&source);
    let mut type_index =
        crate::pipeline_types::TypeIndex::new(root.to_path_buf(), component.to_path_buf());
    let mut input_values = serde_json::Map::new();
    for input in &parsed_inputs {
        if input.has_default {
            continue; // the author's default wins
        }
        if let Some(ty) = &input.generic {
            input_values.insert(
                input.name.clone(),
                crate::pipeline_types::fake_value(ty, &input.name, &mut type_index, 0),
            );
        }
    }

    Ok(ComponentInfo {
        class_name,
        module_class_name,
        template,
        styles,
        strategy,
        required_inputs: extract_required_inputs(&source),
        input_values,
    })
}

/// Source-level scan for `name = input.required<...>()` signal inputs.
/// (Legacy `@Input({ required: true })` is not handled — v22 codebases use
/// signal inputs.)
fn extract_required_inputs(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(rel) = source[from..].find("input.required") {
        let idx = from + rel;
        let before = source[..idx].trim_end();
        if let Some(stripped) = before.strip_suffix('=') {
            if !stripped.ends_with('=') {
                let name: String = stripped
                    .trim_end()
                    .chars()
                    .rev()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                let name: String = name.chars().rev().collect();
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        from = idx + "input.required".len();
    }
    out
}

/// Find `@Component(` and return the byte range of its balanced argument block.
/// String literals are skipped so parens inside them don't break depth counting.
fn extract_decorator_block(source: &str) -> Option<std::ops::Range<usize>> {
    let start = source.find("@Component")? + "@Component".len();
    let rest = &source[start..];
    let open = rest.find('(')?;
    let open = start + open;
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open..(i + 1));
                }
            }
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
            _ => {}
        }
        i += 1;
    }
    None
}

/// Extract a single quoted string value for `key:` inside a decorator block.
fn extract_string_value(block: &str, key: &str) -> Option<String> {
    let idx = find_key(block, key)?;
    let rest = &block[idx..];
    let quote = rest.chars().find(|c| *c == '\'' || *c == '"')?;
    let start = rest.find(quote)? + quote.len_utf8();
    let end = rest[start..].find(quote)? + start;
    Some(rest[start..end].to_string())
}

/// Extract all quoted strings from `key: [ ... ]` inside a decorator block.
fn extract_string_array(block: &str, key: &str) -> Vec<String> {
    let Some(idx) = find_key(block, key) else {
        return Vec::new();
    };
    let rest = &block[idx..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close_rel) = rest[open..].find(']') else {
        return Vec::new();
    };
    let inner = &rest[open + 1..open + close_rel];
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != quote {
                if bytes[end] == b'\\' {
                    end += 1;
                }
                end += 1;
            }
            out.push(inner[start..end].to_string());
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Locate `key` as a property (word boundary followed by optional space then `:`).
fn find_key(block: &str, key: &str) -> Option<usize> {
    let mut from = 0;
    loop {
        let idx = block[from..].find(key)? + from;
        let before_ok = idx == 0
            || !block[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = &block[idx + key.len()..];
        let after_ok = after.trim_start().starts_with(':');
        if before_ok && after_ok {
            return Some(idx);
        }
        from = idx + key.len();
    }
}

/// First `class <Name>` declaration after the decorator ends.
fn class_name_after(source: &str, from: usize) -> Option<String> {
    let rest = &source[from..];
    let idx = rest.find("class ")?;
    let after = &rest[idx + "class ".len()..];
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// First `export class <Name>` in an `*.module.ts` source.
pub fn module_class_name(module_source: &str) -> Option<String> {
    let idx = module_source.find("export class ")?;
    let after = &module_source[idx + "export class ".len()..];
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    const GALLERY: &str = r#"
import { Component } from '@angular/core';

@Component({
  selector: 'app-gallery',
  standalone: true,
  templateUrl: './gallery.component.html',
  styleUrls: ['./gallery.component.scss'],
})
export class GalleryComponent {}
"#;

    #[test]
    fn valid_standalone_component_resolves_files() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("gallery.component.ts");
        write(&comp, GALLERY);
        write(&dir.path().join("gallery.component.html"), "<p>gallery</p>");
        write(
            &dir.path().join("gallery.component.scss"),
            "p { color: red; }",
        );

        let info = validate(&comp, dir.path()).expect("valid component");
        assert_eq!(info.class_name, "GalleryComponent");
        assert_eq!(info.strategy, Strategy::Standalone);
        assert_eq!(
            info.template,
            TemplateRef::File(dir.path().join("gallery.component.html"))
        );
        assert_eq!(info.styles.len(), 1);
    }

    #[test]
    fn module_strategy_when_sibling_module_declares_component() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("badge.component.ts");
        write(
            &comp,
            &GALLERY
                .replace("GalleryComponent", "BadgeComponent")
                .replace("gallery.component", "badge.component"),
        );
        write(&dir.path().join("badge.component.html"), "<p>badge</p>");
        write(
            &dir.path().join("badge.component.scss"),
            "p { color: blue; }",
        );
        write(
            &dir.path().join("badge.module.ts"),
            "@NgModule({}) export class BadgeModule {}",
        );

        let info = validate(&comp, dir.path()).expect("valid module component");
        assert_eq!(info.strategy, Strategy::Module);
        assert_eq!(info.module_class_name.as_deref(), Some("BadgeModule"));
    }

    #[test]
    fn broken_decorator_is_typed_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("broken.component.ts");
        write(
            &comp,
            "@Component({ selector: 'app-broken', templateUrl: 'x.html' export class BrokenComponent {}",
        );
        let err = validate(&comp, dir.path()).unwrap_err();
        assert!(matches!(err, PipelineError::Parse(_)), "got: {err:?}");
    }

    #[test]
    fn missing_template_file_is_early_error() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("ghost.component.ts");
        write(
            &comp,
            &GALLERY.replace("GalleryComponent", "GhostComponent"),
        );
        // gallery.component.html deliberately NOT written.

        let err = validate(&comp, dir.path()).unwrap_err();
        assert!(
            matches!(&err, PipelineError::TemplateMissing(p) if p.ends_with("gallery.component.html")),
            "got: {err:?}"
        );
    }

    #[test]
    fn missing_style_file_is_early_error() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("nostyle.component.ts");
        write(
            &comp,
            &GALLERY.replace("GalleryComponent", "NoStyleComponent"),
        );
        write(&dir.path().join("gallery.component.html"), "<p>x</p>");

        let err = validate(&comp, dir.path()).unwrap_err();
        assert!(
            matches!(&err, PipelineError::StyleMissing(p) if p.ends_with("gallery.component.scss")),
            "got: {err:?}"
        );
    }

    #[test]
    fn source_without_decorator_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("helper.component.ts");
        write(&plain, "export const x = 1;");
        assert!(matches!(
            validate(&plain, dir.path()),
            Err(PipelineError::NotAComponent)
        ));
    }

    #[test]
    fn input_values_generated_from_workspace_interfaces() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        let comp_dir = root.join("gym/projects/app/cal");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("cal.component.ts");
        std::fs::write(
            &comp,
            r#"
import { CalendarUser } from './calendar-user.interface';

@Component({ selector: 'app-cal', template: '<p>{{ user().name }}</p>' })
export class CalComponent {
  user = input.required<CalendarUser>();
  count = model<number>(5);
  items = input<CalendarItem[]>();
  active = input.required<boolean>();
}
"#,
        )
        .unwrap();
        std::fs::write(
            comp_dir.join("calendar-user.interface.ts"),
            "export interface CalendarUser { id: string; name: string; email: string; active: boolean; }",
        )
        .unwrap();

        let info = validate(&comp, root).expect("valid");
        let user = info.input_values.get("user").expect("user value");
        assert_eq!(user["id"], "id-1024");
        assert_eq!(user["name"], "Sample name");
        assert_eq!(user["email"], "user@example.com");
        assert_eq!(user["active"], true);
        // model with author default → not overridden
        assert!(info.input_values.get("count").is_none());
        // unknown interface type → empty array (safe skeleton)
        assert!(info.input_values.contains_key("items"));
        assert_eq!(info.input_values["active"], Value::Bool(true));
    }

    #[test]
    fn required_signal_inputs_are_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("state.component.ts");
        write(
            &comp,
            r#"
@Component({ selector: 'app-state', template: '<div>{{ state.id }}</div>' })
export class StateComponent {
  state = input.required<DashboardState>();
  label = input.required<string>();
  optional = input<string>();
}
"#,
        );
        let info = validate(&comp, dir.path()).expect("valid");
        assert_eq!(
            info.required_inputs,
            vec!["state", "label"],
            "optional excluded"
        );
    }

    #[test]
    fn inline_template_and_styles_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("inline.component.ts");
        write(
            &comp,
            "@Component({ template: '<p>hi</p>', styles: ['p {}'] })\nexport class InlineComponent {}",
        );
        let info = validate(&comp, dir.path()).expect("inline component valid");
        assert_eq!(info.template, TemplateRef::Inline);
        assert_eq!(info.styles, vec![StyleRef::Inline]);
    }
}
