use std::fs;
use std::path::{Path, PathBuf};

/// What kind of source file this is. Drives highlighting (C03/C16) and renderability (C08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    /// Angular component with no sibling `.module.ts` (D3).
    AngularStandalone,
    /// Angular component whose sibling `.module.ts` declares it (D3).
    AngularModule,
    /// An `*.module.ts` file itself: highlighted, but not directly renderable.
    AngularModuleFile,
    React,
    Svelte,
    Vue,
    Other,
}

impl ComponentKind {
    /// Only Angular components are renderable in v1 (D2/D4).
    pub fn renderable(self) -> bool {
        matches!(
            self,
            ComponentKind::AngularStandalone | ComponentKind::AngularModule
        )
    }

    /// Whether the explorer highlights this kind as a component-related file (R4).
    pub fn highlighted(self) -> bool {
        !matches!(self, ComponentKind::Other)
    }

    /// Status-bar message shown when selecting a non-renderable highlighted file (D4).
    pub fn unsupported_message(self) -> Option<&'static str> {
        match self {
            ComponentKind::React => Some("React (.tsx) components are not supported yet"),
            ComponentKind::Svelte => Some("Svelte (.svelte) components are not supported yet"),
            ComponentKind::Vue => Some("Vue (.vue) components are not supported yet"),
            ComponentKind::AngularModuleFile => {
                Some("Module file: select a *.component.ts instead")
            }
            _ => None,
        }
    }
}

/// Classify a source file path by framework/role.
pub fn classify(path: &Path) -> ComponentKind {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return ComponentKind::Other;
    };

    if name.ends_with(".component.ts") {
        let Ok(source) = fs::read_to_string(path) else {
            return ComponentKind::Other;
        };
        if !source.contains("@Component") {
            return ComponentKind::Other;
        }
        if sibling_module(path).is_some() {
            ComponentKind::AngularModule
        } else {
            ComponentKind::AngularStandalone
        }
    } else if name.ends_with(".module.ts") {
        ComponentKind::AngularModuleFile
    } else if name.ends_with(".tsx") {
        ComponentKind::React
    } else if name.ends_with(".svelte") {
        ComponentKind::Svelte
    } else if name.ends_with(".vue") {
        ComponentKind::Vue
    } else {
        ComponentKind::Other
    }
}

/// If `path` is `foo.component.ts`, returns the sibling `foo.module.ts` when it exists (D3).
pub fn sibling_module(component: &Path) -> Option<PathBuf> {
    let name = component.file_name()?.to_str()?;
    let base = name.strip_suffix(".component.ts")?;
    let module_path = component.with_file_name(format!("{base}.module.ts"));
    module_path.is_file().then_some(module_path)
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

    #[test]
    fn standalone_when_no_sibling_module() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("gallery.component.ts");
        write(
            &comp,
            "import { Component } from '@angular/core'; @Component({}) export class GalleryComponent {}",
        );
        assert_eq!(classify(&comp), ComponentKind::AngularStandalone);
    }

    #[test]
    fn module_kind_when_sibling_module_exists() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("badge.component.ts");
        write(&comp, "@Component({}) export class BadgeComponent {}");
        let module = dir.path().join("badge.module.ts");
        write(&module, "export class BadgeModule {}");
        assert_eq!(classify(&comp), ComponentKind::AngularModule);
        assert_eq!(classify(&module), ComponentKind::AngularModuleFile);
    }

    #[test]
    fn ts_without_decorator_is_other() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("helper.component.ts");
        write(&plain, "export const x = 1;");
        assert_eq!(classify(&plain), ComponentKind::Other);
    }

    #[test]
    fn future_framework_extensions_are_highlighted_but_not_renderable() {
        let dir = tempfile::tempdir().unwrap();
        let react = dir.path().join("Button.tsx");
        write(&react, "export const Button = () => null;");
        let svelte = dir.path().join("Card.svelte");
        write(&svelte, "<p>card</p>");
        let vue = dir.path().join("Badge.vue");
        write(&vue, "<template><p></p></template>");

        assert_eq!(classify(&react), ComponentKind::React);
        assert_eq!(classify(&svelte), ComponentKind::Svelte);
        assert_eq!(classify(&vue), ComponentKind::Vue);
        for kind in [
            ComponentKind::React,
            ComponentKind::Svelte,
            ComponentKind::Vue,
        ] {
            assert!(kind.highlighted());
            assert!(!kind.renderable());
            assert!(
                kind.unsupported_message().is_some(),
                "{kind:?} needs a message"
            );
        }
    }

    #[test]
    fn plain_files_are_other_and_inert() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("helper.ts");
        write(&plain, "export const x = 1;");
        let html = dir.path().join("gallery.component.html");
        write(&html, "<p>x</p>");
        assert_eq!(classify(&plain), ComponentKind::Other);
        assert_eq!(classify(&html), ComponentKind::Other);
        assert!(!ComponentKind::Other.highlighted());
        assert!(!ComponentKind::Other.renderable());
    }
}
