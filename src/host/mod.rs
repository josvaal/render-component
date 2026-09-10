//! Embedded Angular host application (D2).
//!
//! The host is materialized into the OS cache dir on first run and served by the
//! Angular dev-server (`ng serve`, `@angular/build`) on a free port. The selected
//! component is injected by regenerating `src/selected/entry.ts`, which re-exports
//! the component (and, for module strategy, its NgModule) through an `external`
//! symlink pointing at the user's project root. Regenerating the entry triggers a
//! dev-server rebuild; the page reloads on revision change (hot-swap, R7).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::Value;

use crate::logs::LogSink;
use crate::pipeline::ComponentInfo;

static TEMPLATE: include_dir::Dir =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/host/template");

pub struct HostContext {
    pub dir: PathBuf,
    pub vite_port: u16,
    pub url: String,
    pub child: tokio::process::Child,
}

/// Extract the embedded template into `dest`, overwriting template files.
pub fn materialize(dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    TEMPLATE.extract(dest)?;
    Ok(())
}

/// The host lives in the OS cache dir, keyed by the target project's Angular
/// major version so different projects get matching toolchains.
pub fn host_dir_for(major: u32) -> anyhow::Result<PathBuf> {
    let base = dirs::cache_dir()
        .context("cannot determine cache directory")?
        .join("render-component")
        .join(format!("host-ng{major}"));
    Ok(base)
}

/// Read the target workspace's `@angular/core` version range from the nearest
/// package.json walking up from the component (falls back to `^20.0.0`).
fn detect_angular_range(root: &Path, component: &Path) -> String {
    let mut dir = component.parent().map(|p| p.to_path_buf());
    loop {
        let Some(current) = dir else { break };
        if let Ok(raw) = fs::read_to_string(current.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                for section in ["dependencies", "devDependencies"] {
                    // NOTE: direct get (not JSON pointer): the key
                    // "@angular/core" itself contains a slash.
                    if let Some(range) = v
                        .get(section)
                        .and_then(|s| s.get("@angular/core"))
                        .and_then(|x| x.as_str())
                    {
                        return range.to_string();
                    }
                }
            }
        }
        if current == root {
            break;
        }
        dir = current.parent().map(|p| p.to_path_buf());
    }
    "^20.0.0".to_string()
}

fn angular_major(range: &str) -> u32 {
    let digits: String = range
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(20)
}

/// Point every `@angular/*` package at the target's version range, and adopt
/// any other version the user's workspace declares (typescript, zone.js,
/// rxjs, ...) — Angular 22 needs typescript 6, Angular 20 needs 5.x, so the
/// user's own constraints are the source of truth.
fn patch_angular_versions(
    dir: &Path,
    range: &str,
    user_versions: &serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    let pkg_path = dir.join("package.json");
    let mut pkg: Value = serde_json::from_str(&fs::read_to_string(&pkg_path)?)?;
    for section in ["dependencies", "devDependencies"] {
        if let Some(map) = pkg
            .pointer_mut(&format!("/{section}"))
            .and_then(|v| v.as_object_mut())
        {
            for (name, version) in map.iter_mut() {
                if name.starts_with("@angular/") {
                    *version = Value::String(range.to_string());
                } else if let Some(user_range) = user_versions.get(name.as_str()) {
                    *version = user_range.clone();
                }
            }
        }
    }
    fs::write(&pkg_path, serde_json::to_string_pretty(&pkg)?)?;
    Ok(())
}

/// All version ranges the user's workspace declares (dependencies +
/// devDependencies merged).
fn collect_user_versions(root: &Path, component: &Path) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let mut dir = component.parent().map(|p| p.to_path_buf());
    loop {
        let Some(current) = dir else { break };
        if let Ok(raw) = fs::read_to_string(current.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                for section in ["dependencies", "devDependencies"] {
                    if let Some(map) = v.get(section).and_then(|s| s.as_object()) {
                        for (name, range) in map {
                            out.entry(name.clone()).or_insert_with(|| range.clone());
                        }
                    }
                }
            }
        }
        if current == root {
            break;
        }
        dir = current.parent().map(|p| p.to_path_buf());
    }
    out
}

/// Modal-content components (opened via `NgbModal.open`) inject `NgbActiveModal`,
/// a token ng-bootstrap only provides inside a modal's dynamic injector — direct
/// instantiation fails with NG0201. A substring scan is enough: the token name
/// can only appear in the source when the component actually references it.
fn needs_modal_stub(component: &Path) -> bool {
    fs::read_to_string(component)
        .map(|src| src.contains("NgbActiveModal"))
        .unwrap_or(false)
}

/// Generate `src/selected/stubs.ts`, imported by main.ts and spread into the
/// bootstrap providers. Same component → same content → no rebuild (C09).
fn write_stubs(selected_dir: &Path, component: &Path) -> anyhow::Result<()> {
    let content = if needs_modal_stub(component) {
        concat!(
            "// Auto-generated by render-component. Do not edit.\n",
            "// Modal-content component: NgbActiveModal only exists inside a real\n",
            "// NgbModal injector, so provide a no-op stub to render the markup flat.\n",
            "import { NgbActiveModal } from '@ng-bootstrap/ng-bootstrap';\n",
            "import type { Provider } from '@angular/core';\n",
            "\n",
            "export const stubProviders: Provider[] = [\n",
            "  { provide: NgbActiveModal, useValue: { close: () => {}, dismiss: () => {} } },\n",
            "];\n",
        )
    } else {
        // No `Provider` annotation and no imports: an empty list needs none,
        // and a type-only import without use would break non-modal builds.
        "// Auto-generated by render-component. Do not edit.\nexport const stubProviders = [];\n"
    };
    let stubs = selected_dir.join("stubs.ts");
    fs::write(&stubs, content)?;
    Ok(())
}

/// Regenerate `src/selected/entry.ts` so the dev server recompiles the new component.
/// Same component → same content → no rebuild (C09).
pub fn write_entry(
    host: &Path,
    component: &Path,
    root: &Path,
    info: &ComponentInfo,
) -> anyhow::Result<PathBuf> {
    let selected_dir = host.join("src").join("selected");
    std::fs::create_dir_all(&selected_dir)?;

    let rel_from_selected = |target: &Path| -> anyhow::Result<String> {
        let rel = target.strip_prefix(root).map_err(|_| {
            anyhow::anyhow!("{} is not under root {}", target.display(), root.display())
        })?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let rel = rel.strip_suffix(".ts").unwrap_or(&rel).to_string();
        Ok(format!("../../external/{rel}"))
    };

    let component_import = rel_from_selected(component)?;
    let mut lines = vec![
        "// Auto-generated by render-component. Do not edit.".to_string(),
        format!(
            "export {{ {} }} from '{component_import}';",
            info.class_name
        ),
    ];
    if let (Some(module_class), Some(module_path)) = (
        &info.module_class_name,
        crate::detect::sibling_module(component),
    ) {
        let module_import = rel_from_selected(&module_path)?;
        lines.push(format!(
            "export {{ {module_class} }} from '{module_import}';"
        ));
    }

    let entry = selected_dir.join("entry.ts");
    let mut f = std::fs::File::create(&entry)?;
    f.write_all(lines.join("\n").as_bytes())?;
    f.write_all(b"\n")?;
    write_stubs(&selected_dir, component)?;
    Ok(entry)
}

/// Point `<host>/external` at the project root so imports resolve inside the
/// host workspace (with `preserveSymlinks: true`) while files stay in place:
/// templateUrl/styleUrls/child imports keep resolving against the real files (R6).
fn link_external(host: &Path, root: &Path) -> anyhow::Result<()> {
    let link = host.join("external");
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir(&link);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root, &link)
            .with_context(|| format!("symlinking {} -> {}", link.display(), root.display()))?;
    }
    #[cfg(not(unix))]
    {
        bail!("symlinked external root is not supported on this platform yet");
    }
    Ok(())
}

fn write_proxy_conf(host: &Path, cli_port: u16) -> anyhow::Result<()> {
    let conf = serde_json::json!({
        "/api": { "target": format!("http://127.0.0.1:{cli_port}"), "secure": false }
    });
    std::fs::write(
        host.join("proxy.conf.json"),
        serde_json::to_string_pretty(&conf)?,
    )?;
    Ok(())
}

/// Reserve a free port, then release it for the dev server to bind (best-effort;
/// `ensure_host` retries once with a fresh port if the dev server fails to come up).
pub fn pick_vite_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Read a child process stream line by line into the log sink (C18).
/// `on_settle` flips when a build settles (complete/failed) — the page-side
/// reload gate waits for it (C31). `on_end` flips when the stream closes,
/// which for the dev server means the process is gone.
fn spawn_line_reader<R>(
    reader: R,
    sink: LogSink,
    tag: String,
    on_settle: Option<Arc<AtomicBool>>,
    on_end: Option<Arc<AtomicBool>>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let clean = strip_ansi(&line);
            if let Some(flag) = &on_settle {
                if clean.contains("Application bundle generation") {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            sink.push(format!("[{tag}] {clean}"));
        }
        if let Some(flag) = on_end {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
}

/// Materialize, install (first run only) and link the external root. Split from
/// `spawn_devserver` so the caller can write the selection entry BEFORE the
/// dev server builds (a missing entry fails the first compile).
pub async fn prepare_host(
    root: &Path,
    component: &Path,
    cli_port: u16,
    sink: &LogSink,
) -> anyhow::Result<PathBuf> {
    let range = detect_angular_range(root, component);
    let major = angular_major(&range);
    sink.push(format!(
        "[host] target project Angular range: {range} (host-ng{major})"
    ));
    let user_versions = collect_user_versions(root, component);
    let dir = host_dir_for(major)?;
    materialize(&dir).context("materializing host template")?;
    patch_angular_versions(&dir, &range, &user_versions)?;
    write_proxy_conf(&dir, cli_port)?;
    link_external(&dir, root)?;
    bridge_tsconfig_paths(root, component, &dir, sink);
    bridge_global_styles(root, component, &dir, sink);
    generate_globals(root, &dir, sink);

    let node_modules = dir.join("node_modules");
    if !node_modules.is_dir() {
        sink.push("[npm] first run: installing host dependencies (this can take a while)...");
        let mut child = tokio::process::Command::new("npm")
            .args([
                "install",
                "--no-progress",
                "--no-audit",
                "--no-fund",
                "--legacy-peer-deps",
            ])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning npm install (is Node.js installed?)")?;
        let stdout = child.stdout.take().context("npm stdout")?;
        let stderr = child.stderr.take().context("npm stderr")?;
        let t1 = spawn_line_reader(stdout, sink.clone(), "npm".into(), None, None);
        let t2 = spawn_line_reader(stderr, sink.clone(), "npm!".into(), None, None);
        let status = child.wait().await?;
        let _ = t1.await;
        let _ = t2.await;
        if !status.success() {
            bail!("npm install failed with {status} — see the logs above");
        }
        sink.push("[npm] install finished");
    }
    install_project_dependencies(root, component, &dir, sink).await;
    Ok(dir)
}

/// Spawn `ng serve` on a free port and wait until it answers. All output is
/// streamed into the log sink. `build_pending` is cleared when a build
/// settles; `ng_exited` flips if the dev-server process dies.
pub async fn spawn_devserver(
    dir: &Path,
    sink: &LogSink,
    build_pending: Arc<AtomicBool>,
    ng_exited: Arc<AtomicBool>,
) -> anyhow::Result<HostContext> {
    let mut last_err = None;
    for attempt in 1..=2 {
        let vite_port = pick_vite_port()?;
        sink.push(format!(
            "[ng] starting dev-server (attempt {attempt}, port {vite_port})..."
        ));
        let mut child = tokio::process::Command::new("npx")
            .args([
                "ng",
                "serve",
                "--port",
                &vite_port.to_string(),
                "--host",
                "127.0.0.1",
                "--proxy-config",
                "proxy.conf.json",
            ])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawning ng serve (is Node.js installed?)")?;
        let stdout = child.stdout.take().context("ng stdout")?;
        let stderr = child.stderr.take().context("ng stderr")?;
        let t1 = spawn_line_reader(
            stdout,
            sink.clone(),
            "ng".into(),
            Some(build_pending.clone()),
            Some(ng_exited.clone()),
        );
        let t2 = spawn_line_reader(stderr, sink.clone(), "ng!".into(), None, None);

        let url = format!("http://127.0.0.1:{vite_port}");
        let wait = wait_ready(&url, Duration::from_secs(240)).await;
        // Keep streaming whatever remains; tasks end when the pipes close.
        let _ = t1.is_finished();
        let _ = t2.is_finished();
        match wait {
            Ok(()) => {
                sink.push(format!("[ng] dev server ready at {url}"));
                return Ok(HostContext {
                    dir: dir.to_path_buf(),
                    vite_port,
                    url,
                    child,
                });
            }
            Err(e) => {
                let _ = child.kill().await;
                sink.push(format!("[ng!] attempt {attempt} failed: {e}"));
                last_err = Some(e);
            }
        }
    }
    bail!(
        "Angular dev-server did not become ready: {} — check the logs above",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    )
}

/// Strip JSONC noise (line/block comments, trailing commas) from a tsconfig.
/// `//` is only treated as a comment when not part of a URL (`://`).
fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    while let Some(&n) = chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
                Some('*') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => out.push('/'),
            },
            _ => out.push(c),
        }
    }
    // Trailing commas: drop a ',' when the next non-whitespace char closes a block.
    let mut cleaned = String::with_capacity(out.len());
    let bytes: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == ',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == '}' || bytes[j] == ']') {
                i += 1;
                continue;
            }
        }
        cleaned.push(c);
        i += 1;
    }
    cleaned
}

/// Resolve a tsconfig `extends` value (relative or package-style) to a file.
fn resolve_extends(from: &Path, ext: &str) -> Option<PathBuf> {
    let mut base_file = from.parent().unwrap_or(Path::new(".")).join(ext);
    if base_file.extension().is_none() {
        base_file.set_extension("json");
    }
    if base_file.is_file() {
        return Some(base_file);
    }
    // Package-style extends: try the workspace node_modules next to the file.
    let pkg = from
        .parent()
        .unwrap_or(Path::new("."))
        .join("node_modules")
        .join(ext);
    let pkg = if pkg.extension().is_none() {
        pkg.with_extension("json")
    } else {
        pkg
    };
    pkg.is_file().then_some(pkg)
}

/// Raw values along the extends chain, child first (parent last).
fn tsconfig_chain(config_file: &Path, max_depth: usize) -> Vec<(PathBuf, Value)> {
    let mut chain: Vec<(PathBuf, Value)> = Vec::new();
    let mut current = Some(config_file.to_path_buf());
    while let Some(file) = current {
        if chain.len() >= max_depth {
            break;
        }
        let Ok(raw) = fs::read_to_string(&file) else {
            break;
        };
        let Ok(value) = serde_json::from_str::<Value>(&strip_jsonc(&raw)) else {
            break;
        };
        current = value
            .get("extends")
            .and_then(|e| e.as_str())
            .and_then(|ext| resolve_extends(&file, ext));
        chain.push((file, value));
    }
    chain
}

/// Collect ambient declaration files (`*.d.ts`) declared via `files`/`include`
/// in the config chain, mapped to `external/...` host paths. Glob entries are
/// kept as globs; concrete entries must exist under the project root.
fn declarations_from_configs(root: &Path, config_file: &Path, out: &mut Vec<String>) {
    for (file, value) in tsconfig_chain(config_file, 6) {
        for key in ["files", "include"] {
            let Some(entries) = value.pointer(&format!("/{key}")).and_then(|v| v.as_array()) else {
                continue;
            };
            for entry in entries {
                let Some(raw) = entry.as_str() else { continue };
                if !raw.ends_with(".d.ts") && !raw.ends_with("prototypes.ts") {
                    continue;
                }
                let joined = file.parent().unwrap_or(Path::new(".")).join(raw);
                let joined = joined.to_string_lossy().replace('\\', "/");
                let joined = joined.strip_prefix("./").unwrap_or(&joined).to_string();
                let Some(rel) =
                    joined.strip_prefix(&format!("{}/", root.to_string_lossy().replace('\\', "/")))
                else {
                    continue; // outside the explored root: cannot bridge
                };
                let mapped = format!("external/{rel}");
                if !out.contains(&mapped) {
                    if raw.contains('*') || root.join(rel).is_file() {
                        out.push(mapped);
                    }
                }
            }
        }
    }
}

/// Fallback: bounded walk for `*.d.ts` under the project root when the
/// tsconfig declares none (skips node_modules/dist/build caches).
fn glob_ambient_declarations(root: &Path, out: &mut Vec<String>) {
    for entry in walkdir::WalkDir::new(root)
        .max_depth(8)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), "node_modules" | "dist" | ".git" | ".angular")
        })
    {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let is_ambient = name.ends_with(".d.ts") || name == "prototypes.ts";
        if is_ambient && out.len() < 120 {
            let rel = entry
                .path()
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let mapped = format!("./external/{rel}");
            if !out.contains(&mapped) {
                out.push(mapped);
            }
        }
    }
}

/// Load a tsconfig following its `extends` chain (max depth 5), merging
/// `compilerOptions.paths` (child wins) and inheriting `baseUrl`.
fn load_tsconfig_merged(file: &Path, depth: usize) -> Option<Value> {
    if depth > 5 {
        return None;
    }
    let raw = fs::read_to_string(file).ok()?;
    let value: Value = serde_json::from_str(&strip_jsonc(&raw)).ok()?;
    let mut merged = value.clone();
    if let Some(ext) = value.get("extends").and_then(|e| e.as_str()) {
        let mut base_file = file.parent().unwrap_or(Path::new(".")).join(ext);
        if base_file.extension().is_none() {
            base_file.set_extension("json");
        }
        if !base_file.is_file() {
            // Package-style extends: try the workspace node_modules next to the file.
            let pkg = file
                .parent()
                .unwrap_or(Path::new("."))
                .join("node_modules")
                .join(ext);
            let pkg = if pkg.extension().is_none() {
                pkg.with_extension("json")
            } else {
                pkg
            };
            base_file = pkg;
        }
        if let Some(base) = load_tsconfig_merged(&base_file, depth + 1) {
            // TS `extends` semantics: base fields win unless the child redefines
            // them; `compilerOptions`/`angularCompilerOptions` merge per-key.
            let mut out = base.as_object().cloned().unwrap_or_default();
            if let Some(child_obj) = merged.as_object().cloned() {
                for (key, child_value) in child_obj {
                    match (
                        key.as_str(),
                        child_value.as_object(),
                        out.get_mut(&key).and_then(|e| e.as_object_mut()),
                    ) {
                        (
                            section @ ("compilerOptions" | "angularCompilerOptions"),
                            Some(child_map),
                            Some(base_map),
                        ) => {
                            for (ck, cv) in child_map {
                                base_map.insert(ck.clone(), cv.clone());
                            }
                            let _ = section;
                        }
                        _ => {
                            out.insert(key, child_value);
                        }
                    }
                }
            }
            merged = Value::Object(out);
        }
    }
    Some(merged)
}

/// Type-checking flags inherited from the user workspace's tsconfig (see
/// `bridge_tsconfig_paths`). Build/module plumbing stays ours.
const BRIDGED_COMPILER_OPTIONS: &[&str] = &[
    "strict",
    "noImplicitAny",
    "strictNullChecks",
    "strictFunctionTypes",
    "strictBindCallApply",
    "strictPropertyInitialization",
    "noImplicitThis",
    "useUnknownInCatchVariables",
    "alwaysStrict",
    "exactOptionalPropertyTypes",
    "noImplicitReturns",
    "noFallthroughCasesInSwitch",
    "noImplicitOverride",
    "experimentalDecorators",
    "emitDecoratorMetadata",
    "useDefineForClassFields",
    "target",
    "lib",
    "esModuleInterop",
    "allowSyntheticDefaultImports",
    "resolveJsonModule",
    "skipLibCheck",
    "isolatedModules",
];

/// Find the tsconfig that governs `component`:
/// 1. a dir containing `angular.json` is the canonical Angular workspace root
///    — its tsconfig.json wins (workspaces nest arbitrarily deep, e.g.
///    `gym/gym/projects/system-modules/...`);
/// 2. otherwise the nearest `tsconfig.json` walking up.
/// The walk is uncapped and bounded by the explored root.
fn find_workspace_config(root: &Path, component: &Path) -> Option<PathBuf> {
    let mut nearest_ts: Option<PathBuf> = None;
    let mut dir = component.parent().map(|p| p.to_path_buf());
    loop {
        let Some(current) = dir else { break };
        let ts = current.join("tsconfig.json");
        if nearest_ts.is_none() && ts.is_file() {
            nearest_ts = Some(ts.clone());
        }
        if current.join("angular.json").is_file() && ts.is_file() {
            return Some(ts);
        }
        if current == root {
            break;
        }
        dir = current.parent().map(|p| p.to_path_buf());
    }
    nearest_ts
}

/// Strip ANSI escape sequences (colors etc.) from a streamed compiler line so
/// the log panel and dumps stay readable/shareable.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip CSI sequences: ESC [ ... final-byte (@-~)
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
                continue;
            }
            // Other escape introducers: skip one byte.
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

/// Bridge the user workspace's tsconfig `paths` into the host tsconfig so
/// path aliases in their components resolve (prefix targets with `external/`,
/// which symlinks back to the project root). Framework deps are filtered out.
fn bridge_tsconfig_paths(root: &Path, component: &Path, host_dir: &Path, sink: &LogSink) {
    let Some(config_file) = find_workspace_config(root, component) else {
        sink.push(
            "[host] no tsconfig.json found near the component — path aliases will not resolve",
        );
        return;
    };

    let Some(merged) = load_tsconfig_merged(&config_file, 0) else {
        sink.push(format!(
            "[host!] could not parse {} (tsconfig) — path aliases will not resolve",
            config_file.display()
        ));
        return;
    };

    // `paths` targets resolve against baseUrl, which is relative to the CONFIG
    // FILE's directory — not the explored root. Re-anchor under external/.
    let base_url = merged
        .pointer("/compilerOptions/baseUrl")
        .and_then(|b| b.as_str())
        .unwrap_or(".")
        .to_string();
    let config_dir = config_file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let paths = merged
        .pointer("/compilerOptions/paths")
        .and_then(|p| p.as_object());

    // Stage 1: bridge path aliases (may be empty — workspace base configs
    // often carry none; the other stages still apply).
    let mut bridged = serde_json::Map::new();
    let mut skipped_framework = 0usize;
    if let Some(paths) = paths {
        for (key, values) in paths {
            if key.starts_with("@angular") || key == "rxjs" || key == "tslib" || key == "zone.js" {
                skipped_framework += 1;
                continue;
            }
            let Some(vals) = values.as_array() else {
                continue;
            };
            let mapped: Vec<String> = vals
                .iter()
                .filter_map(|v| v.as_str())
                .map(|v| {
                    let base = if base_url == "." {
                        PathBuf::new()
                    } else {
                        PathBuf::from(&base_url)
                    };
                    let abs = if base.is_absolute() {
                        base.join(v)
                    } else {
                        config_dir.join(base).join(v)
                    };
                    match abs.strip_prefix(root) {
                        Ok(rel) => {
                            format!("./external/{}", rel.to_string_lossy().replace('\\', "/"))
                        }
                        Err(_) => {
                            format!("./external/{}", v.replace('\\', "/"))
                        }
                    }
                })
                .collect();
            if !mapped.is_empty() {
                bridged.insert(
                    key.clone(),
                    Value::Array(mapped.into_iter().map(Value::String).collect()),
                );
            }
        }
    }

    let host_ts = host_dir.join("tsconfig.json");
    let raw = match fs::read_to_string(&host_ts) {
        Ok(raw) => raw,
        Err(e) => {
            sink.push(format!("[host!] cannot read host tsconfig: {e}"));
            return;
        }
    };
    let mut tsconfig: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            sink.push(format!("[host!] cannot parse host tsconfig: {e}"));
            return;
        }
    };

    if bridged.is_empty() {
        sink.push("[host] no bridgable tsconfig path mappings found");
    } else {
        tsconfig["compilerOptions"]["paths"] = Value::Object(bridged.clone());
        // NOTE: no baseUrl — TS 6 deprecates it and `external/...` targets are
        // already relative to the host tsconfig location.
        sink.push(format!(
            "[host] bridged {} tsconfig path mapping(s) from {} ({} framework keys skipped)",
            bridged.len(),
            config_file.display(),
            skipped_framework
        ));
    }

    // Stage 2: bridge type-checking compiler options so components compile
    // under the SAME strictness as their own workspace (e.g. strict:false
    // projects would otherwise explode with TS2571 under the host's strict).
    let mut bridged_flags: Vec<&str> = Vec::new();
    for flag in BRIDGED_COMPILER_OPTIONS {
        if let Some(value) = merged.pointer(&format!("/compilerOptions/{flag}")) {
            tsconfig["compilerOptions"][*flag] = value.clone();
            bridged_flags.push(flag);
        }
    }
    sink.push(format!(
        "[host] bridged compiler options: {}",
        if bridged_flags.is_empty() {
            "none found".to_string()
        } else {
            bridged_flags.join(", ")
        }
    ));

    // Stage 3: ambient declarations (global d.ts: monkey-patches like
    // String.prototype extensions) are never imported, so they must be
    // included explicitly or their types simply do not exist.
    let mut declarations: Vec<String> = Vec::new();
    declarations_from_configs(root, &config_file, &mut declarations);
    if declarations.is_empty() {
        glob_ambient_declarations(root, &mut declarations);
        if !declarations.is_empty() {
            sink.push(format!(
                "[host] tsconfig declares no d.ts entries — globbed {} from the project",
                declarations.len()
            ));
        }
    }
    if declarations.is_empty() {
        sink.push("[host] no ambient declaration files found (globals like prototype patches will not type-check)");
    } else {
        tsconfig["files"] = Value::Array(
            declarations
                .iter()
                .map(|f| Value::String(f.clone()))
                .collect(),
        );
        sink.push(format!(
            "[host] included {} ambient declaration file(s)",
            declarations.len()
        ));
    }

    if let Err(e) =
        serde_json::to_string_pretty(&tsconfig).map(|pretty| fs::write(&host_ts, pretty))
    {
        sink.push(format!("[host!] cannot write host tsconfig: {e}"));
    }
}

fn find_package_json_for(root: &Path, component: &Path) -> Option<PathBuf> {
    let mut dir = component.parent().map(|p| p.to_path_buf());
    loop {
        let Some(current) = dir else { break };
        let candidate = current.join("package.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        if current == root {
            break;
        }
        dir = current.parent().map(|p| p.to_path_buf());
    }
    None
}

/// Runtime dependency specifiers of the project (`name@range`), taken from
/// the nearest package.json `dependencies` section. These get installed into
/// the host so Vite can resolve every third-party import the component graph
/// uses, with the SAME versions the project pins (transitives included).
fn project_dependency_install_args(root: &Path, component: &Path) -> Vec<String> {
    let Some(pkg_file) = find_package_json_for(root, component) else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(&pkg_file) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(map) = v.get("dependencies").and_then(|d| d.as_object()) else {
        return Vec::new();
    };
    let mut args: Vec<String> = map
        .iter()
        .filter_map(|(name, range)| {
            let range = range.as_str().unwrap_or("latest");
            Some(format!("{name}@{range}"))
        })
        .collect();
    args.sort();
    args.truncate(120); // bounded: workspaces with huge dep lists still work
    args
}

/// Install the project's runtime dependencies into the host node_modules.
/// Batch first (fast); on failure, retry one-by-one so a single private or
/// unavailable package doesn't sink the whole set — each skip is logged.
async fn install_project_dependencies(
    root: &Path,
    component: &Path,
    host_dir: &Path,
    sink: &LogSink,
) {
    let args = project_dependency_install_args(root, component);
    if args.is_empty() {
        return;
    }
    sink.push(format!(
        "[npm] installing {} project runtime dependency(ies)...",
        args.len()
    ));

    let run = |pkgs: &[String]| {
        let mut cmd = tokio::process::Command::new("npm");
        cmd.arg("install")
            .args(pkgs)
            .args([
                "--no-progress",
                "--no-audit",
                "--no-fund",
                "--legacy-peer-deps",
            ])
            .current_dir(host_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    };

    let Ok(mut child) = run(&args).spawn() else {
        sink.push("[npm!] cannot spawn npm for project dependencies");
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        sink.push("[npm!] cannot capture npm stdout");
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        sink.push("[npm!] cannot capture npm stderr");
        return;
    };
    let t1 = spawn_line_reader(stdout, sink.clone(), "npm".into(), None, None);
    let t2 = spawn_line_reader(stderr, sink.clone(), "npm!".into(), None, None);
    let status = child.wait().await;
    let _ = t1.is_finished();
    let _ = t2.is_finished();
    match status {
        Ok(st) if st.success() => {
            sink.push("[npm] project dependencies installed");
            return;
        }
        Ok(st) => sink.push(format!(
            "[npm!] batch install of project deps failed ({st}) — retrying one-by-one"
        )),
        Err(e) => sink.push(format!(
            "[npm!] batch install error: {e} — retrying one-by-one"
        )),
    }

    let mut ok = 0usize;
    let mut failed = 0usize;
    for pkg in &args {
        let status = tokio::process::Command::new("npm")
            .arg("install")
            .arg(pkg)
            .args([
                "--no-progress",
                "--no-audit",
                "--no-fund",
                "--legacy-peer-deps",
            ])
            .current_dir(host_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await;
        match status {
            Ok(out) if out.status.success() => ok += 1,
            _ => {
                failed += 1;
                sink.push(format!(
                    "[npm!] skipped {pkg} (install failed — private package?)"
                ));
            }
        }
    }
    sink.push(format!(
        "[npm] project dependencies: {ok} installed, {failed} skipped"
    ));
}

/// Generate `src/selected/globals.ts` with side-effect imports for project
/// global modules (e.g. `prototypes.ts` with String.prototype patches). It is
/// statically imported by main.ts, so types AND runtime behavior land.
fn generate_globals(root: &Path, host_dir: &Path, sink: &LogSink) {
    let mut files: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(8)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), "node_modules" | "dist" | ".git" | ".angular")
        })
    {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        if entry.file_name().to_string_lossy() == "prototypes.ts" && files.len() < 10 {
            let rel = entry
                .path()
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let rel = rel.strip_suffix(".ts").unwrap_or(&rel).to_string();
            let import = format!("../../external/{rel}");
            if !files.contains(&import) {
                files.push(import);
            }
        }
    }
    let path = host_dir.join("src").join("selected").join("globals.ts");
    if files.is_empty() {
        let _ = fs::write(&path, "// no project globals detected\n");
        return;
    }
    let content = files
        .iter()
        .map(|f| format!("import '{f}';"))
        .collect::<Vec<_>>()
        .join("\n");
    match fs::write(&path, content + "\n") {
        Ok(()) => sink.push(format!(
            "[host] globals: {} project global module(s) loaded at runtime",
            files.len()
        )),
        Err(e) => sink.push(format!("[host!] cannot write globals.ts: {e}")),
    }
}

/// Bridge the workspace's global stylesheets (`angular.json` → build options
/// `styles`) and Sass `stylePreprocessorOptions.includePaths` into the host —
/// components depend on the design-system SCSS the app loads globally.
fn bridge_global_styles(root: &Path, component: &Path, host_dir: &Path, sink: &LogSink) {
    // Find the nearest angular.json walking up from the component, bounded by root.
    let mut dir = component.parent().map(|p| p.to_path_buf());
    let mut ws_file: Option<PathBuf> = None;
    loop {
        let Some(current) = dir else { break };
        let candidate = current.join("angular.json");
        if candidate.is_file() {
            ws_file = Some(candidate);
            break;
        }
        if current == root {
            break;
        }
        dir = current.parent().map(|p| p.to_path_buf());
    }
    let Some(ws_file) = ws_file else {
        sink.push("[host] no angular.json found — global workspace styles will not load");
        return;
    };
    let ws_dir = ws_file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let Ok(raw) = fs::read_to_string(&ws_file) else {
        sink.push("[host!] cannot read angular.json");
        return;
    };
    let Ok(angular) = serde_json::from_str::<Value>(&strip_jsonc(&raw)) else {
        sink.push("[host!] cannot parse angular.json — global styles will not load");
        return;
    };

    // Re-anchor a workspace-relative path to the host (external/<rel>).
    let reanchor = |entry: &str| -> Option<String> {
        let abs = ws_dir.join(entry);
        match abs.strip_prefix(root) {
            Ok(rel) => Some(format!(
                "./external/{}",
                rel.to_string_lossy().replace('\\', "/")
            )),
            Err(_) => None,
        }
    };

    // Pick the project whose root is the longest prefix of the component path.
    let comp_rel = component
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let projects = angular.get("projects").and_then(|p| p.as_object());
    let Some(projects) = projects else {
        sink.push("[host!] angular.json has no projects — global styles will not load");
        return;
    };
    let mut best: Option<(&String, &Value, usize)> = None;
    for (name, project) in projects {
        let proj_root = project.get("root").and_then(|r| r.as_str()).unwrap_or("");
        if comp_rel.starts_with(proj_root) {
            let len = proj_root.len();
            if best.map(|(_, _, l)| len > l).unwrap_or(true) {
                best = Some((name, project, len));
            }
        }
    }
    let Some((project_name, project, _)) =
        best.or_else(|| projects.iter().next().map(|(n, p)| (n, p, 0usize)))
    else {
        return;
    };

    let build_options = project
        .pointer("/architect/build/options")
        .or_else(|| project.pointer("/targets/build/options"));

    // Global stylesheets: string entries or {input} objects (inject:false skipped).
    let mut mapped_styles: Vec<String> = Vec::new();
    if let Some(styles) = build_options
        .and_then(|o| o.get("styles"))
        .and_then(|s| s.as_array())
    {
        for entry in styles {
            let path = match entry {
                Value::String(p) => Some(p.clone()),
                Value::Object(o) => {
                    let inject_false = o.get("inject").and_then(|i| i.as_bool()) == Some(false);
                    if inject_false {
                        None
                    } else {
                        o.get("input").and_then(|i| i.as_str()).map(String::from)
                    }
                }
                _ => None,
            };
            let Some(path) = path else { continue };
            // angular.json entries can point at files that do not exist in
            // this checkout — a missing @import breaks the whole build, so
            // skip them (same policy as the d.ts bridging).
            if !ws_dir.join(&path).is_file() {
                sink.push(format!(
                    "[host] angular.json style '{path}' does not exist — skipped"
                ));
                continue;
            }
            if let Some(mapped) = reanchor(&path) {
                if !mapped_styles.contains(&mapped) {
                    mapped_styles.push(mapped);
                }
            }
        }
    }

    // Sass include paths for @use/@import of shared partials.
    let mut mapped_includes: Vec<String> = Vec::new();
    if let Some(include_paths) = build_options
        .and_then(|o| o.pointer("/stylePreprocessorOptions/includePaths"))
        .and_then(|s| s.as_array())
    {
        for entry in include_paths {
            if let Some(mapped) = entry.as_str().and_then(|e| reanchor(e)) {
                if !mapped_includes.contains(&mapped) {
                    mapped_includes.push(mapped);
                }
            }
        }
    }

    if mapped_styles.is_empty() && mapped_includes.is_empty() {
        sink.push("[host] angular.json declares no global styles or include paths to bridge");
        return;
    }

    let host_angular_path = host_dir.join("angular.json");
    let Ok(host_raw) = fs::read_to_string(&host_angular_path) else {
        sink.push("[host!] cannot read host angular.json");
        return;
    };
    let Ok(mut host_angular) = serde_json::from_str::<Value>(&host_raw) else {
        sink.push("[host!] cannot parse host angular.json");
        return;
    };
    let host_styles_ptr = "/projects/host/architect/build/options/styles";
    let mut styles: Vec<Value> = host_angular
        .pointer(host_styles_ptr)
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    for st in &mapped_styles {
        if !styles.iter().any(|e| e.as_str() == Some(st.as_str())) {
            styles.push(Value::String(st.clone()));
        }
    }
    host_angular["projects"]["host"]["architect"]["build"]["options"]["styles"] =
        Value::Array(styles);

    if !mapped_includes.is_empty() {
        host_angular["projects"]["host"]["architect"]["build"]["options"]["stylePreprocessorOptions"] =
            serde_json::json!({ "includePaths": mapped_includes });
    }

    match serde_json::to_string_pretty(&host_angular)
        .map(|pretty| fs::write(&host_angular_path, pretty))
    {
        Ok(Ok(())) => sink.push(format!(
            "[host] bridged {} global stylesheet(s) and {} include path(s) from project '{project_name}'",
            mapped_styles.len(),
            mapped_includes.len()
        )),
        Ok(Err(e)) => sink.push(format!("[host!] cannot write host angular.json: {e}")),
        Err(e) => sink.push(format!("[host!] cannot serialize host angular.json: {e}")),
    }
}

/// Poll the dev-server URL until it answers HTTP 200.
async fn wait_ready(url: &str, timeout: Duration) -> anyhow::Result<()> {
    let (_, port) = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once(':'))
        .map(|(h, p)| (h.to_string(), p.to_string()))
        .context("bad dev-server url")?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if http_ok("127.0.0.1", &port).is_some() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("timeout waiting for {url}");
}

/// Minimal blocking HTTP/1.0 GET: returns Some(status_line) when the socket answers.
fn http_ok(host: &str, port: &str) -> Option<String> {
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect((host, port.parse::<u16>().ok()?)).ok()?;
    stream
        .write_all(format!("GET / HTTP/1.0\r\nHost: {host}\r\n\r\n").as_bytes())
        .ok()?;
    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Open the default browser at `url` (best effort).
pub fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/c", "start", url]);
        c
    };
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
    #[allow(unreachable_code)]
    {
        let _ = url;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Strategy;

    fn info() -> ComponentInfo {
        ComponentInfo {
            class_name: "GalleryComponent".into(),
            module_class_name: Some("GalleryModule".into()),
            template: crate::pipeline::TemplateRef::Inline,
            styles: vec![],
            strategy: Strategy::Module,
            required_inputs: vec![],
            input_values: serde_json::Map::new(),
        }
    }

    #[test]
    fn materialize_writes_expected_tree() {
        let dir = tempfile::tempdir().unwrap();
        materialize(dir.path()).unwrap();
        for rel in [
            "package.json",
            "angular.json",
            "tsconfig.json",
            "src/main.ts",
            "src/index.html",
        ] {
            assert!(dir.path().join(rel).is_file(), "missing {rel}");
        }
    }

    #[test]
    fn entry_exports_component_and_module() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let comp = root.join("gallery/gallery.component.ts");
        std::fs::create_dir_all(comp.parent().unwrap()).unwrap();
        std::fs::write(&comp, "x").unwrap();
        // Module strategy: sibling module must exist for the second export (D3).
        std::fs::write(root.join("gallery/gallery.module.ts"), "x").unwrap();

        let entry = write_entry(dir.path(), &comp, &root, &info()).unwrap();
        let content = std::fs::read_to_string(entry).unwrap();
        assert!(
            content.contains(
                "export { GalleryComponent } from '../../external/gallery/gallery.component';"
            ),
            "got: {content}"
        );
        assert!(
            content
                .contains("export { GalleryModule } from '../../external/gallery/gallery.module';"),
            "got: {content}"
        );
    }

    #[test]
    fn strip_jsonc_removes_comments_and_trailing_commas_but_keeps_urls() {
        let input = r#"{
  // line comment
  /* block
     comment */
  "compilerOptions": {
    "baseUrl": ".", // trailing comment
    "paths": {
      "@lib/*": ["projects/lib/src/*",],
      "docs": ["https://example.com/docs"],
    },
  },
}"#;
        let v: Value = serde_json::from_str(&strip_jsonc(input)).expect("valid JSON after strip");
        assert_eq!(v["compilerOptions"]["baseUrl"], ".");
        assert!(v["compilerOptions"]["paths"]["@lib/*"].is_array());
        assert_eq!(
            v["compilerOptions"]["paths"]["docs"][0],
            "https://example.com/docs"
        );
    }

    #[test]
    fn find_workspace_config_reaches_deep_nesting_via_angular_json() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        // 9 levels deep: root/gym/projects/system-modules/document-management/ui/components/ui-payment-code/ui-payment-code/ui-payment-code-detail/
        let comp_dir = root
            .join("gym/projects/system-modules/document-management/ui/components/ui-payment-code/ui-payment-code/ui-payment-code-detail");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.component.ts");
        std::fs::write(&comp, "export class X {}").unwrap();
        // Workspace root at root/gym with angular.json + tsconfig.json.
        std::fs::create_dir_all(root.join("gym")).unwrap();
        std::fs::write(root.join("gym/angular.json"), "{}").unwrap();
        std::fs::write(root.join("gym/tsconfig.json"), "{}").unwrap();

        let found = find_workspace_config(root, &comp).expect("workspace tsconfig found");
        assert_eq!(found, root.join("gym/tsconfig.json"));
    }

    #[test]
    fn find_workspace_config_falls_back_to_nearest_tsconfig() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        let comp_dir = root.join("deep/a/b/c");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.ts");
        std::fs::write(&comp, "").unwrap();
        // Intermediate tsconfig (no angular.json anywhere).
        std::fs::create_dir_all(root.join("deep/a")).unwrap();
        std::fs::write(root.join("deep/a/tsconfig.json"), "{}").unwrap();

        let found = find_workspace_config(root, &comp).expect("nearest tsconfig found");
        assert_eq!(found, root.join("deep/a/tsconfig.json"));
    }

    #[test]
    fn detect_angular_range_reads_nearest_package_json() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        let comp_dir = root.join("gym/projects/app/src");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.component.ts");
        std::fs::write(&comp, "export class X {}").unwrap();
        std::fs::write(
            root.join("gym/package.json"),
            r#"{ "dependencies": { "@angular/core": "^22.0.5" } }"#,
        )
        .unwrap();

        assert_eq!(detect_angular_range(root, &comp), "^22.0.5");
        // Default when nothing declares Angular anywhere on the walk.
        assert_eq!(
            detect_angular_range(root, root.join("other.ts").as_path()),
            "^20.0.0"
        );
    }

    #[test]
    fn strip_ansi_removes_color_codes_but_keeps_text() {
        let raw = "\u{1b}[31m\u{1b}[41;31m✘ \u{1b}[1mERROR\u{1b}[0m: TS2307\u{1b}[0m";
        assert_eq!(strip_ansi(raw), "✘ ERROR: TS2307");
        assert_eq!(strip_ansi("plain line"), "plain line");
    }

    #[test]
    fn bridge_global_styles_maps_nearest_project() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        let comp_dir = root.join("gym/projects/app/src/lib");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.component.ts");
        std::fs::write(&comp, "export class X {}").unwrap();
        // The bridged style files must actually exist (missing ones are skipped).
        std::fs::create_dir_all(root.join("gym/assets/scss-v2")).unwrap();
        std::fs::write(root.join("gym/styles.scss"), "").unwrap();
        std::fs::write(root.join("gym/assets/scss-v2/main.scss"), "").unwrap();
        std::fs::write(
            root.join("gym/angular.json"),
            r#"{
  "projects": {
    "other": {
      "root": "other",
      "architect": { "build": { "options": { "styles": ["other-only.scss"] } } }
    },
    "app": {
      "root": "",
      "architect": {
        "build": {
          "options": {
            "styles": ["styles.scss", { "input": "assets/scss-v2/main.scss" }],
            "stylePreprocessorOptions": { "includePaths": ["assets"] }
          }
        }
      }
    }
  }
}"#,
        )
        .unwrap();

        let host_tmp = tempfile::tempdir().unwrap();
        materialize(host_tmp.path()).unwrap();
        let sink = LogSink::new(false);
        bridge_global_styles(root, &comp, host_tmp.path(), &sink);

        let bridged: Value = serde_json::from_str(
            &std::fs::read_to_string(host_tmp.path().join("angular.json")).unwrap(),
        )
        .unwrap();
        let styles: Vec<String> = bridged
            .pointer("/projects/host/architect/build/options/styles")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .expect("styles present");
        // Workspace nested at root/gym → external/gym/... is correct.
        assert!(
            styles.contains(&"./external/gym/styles.scss".to_string()),
            "{styles:?}"
        );
        assert!(
            styles.contains(&"./external/gym/assets/scss-v2/main.scss".to_string()),
            "{styles:?}"
        );
        assert!(
            !styles.iter().any(|s| s.contains("other-only")),
            "longest-prefix project wins"
        );
        let includes: Vec<String> = bridged
            .pointer("/projects/host/architect/build/options/stylePreprocessorOptions/includePaths")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .expect("includePaths present");
        assert!(
            includes.contains(&"./external/gym/assets".to_string()),
            "{includes:?}"
        );
    }

    #[test]
    fn bridge_global_styles_skips_missing_files() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        let comp_dir = root.join("ws/projects/app/src");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.component.ts");
        std::fs::write(&comp, "export class X {}").unwrap();
        std::fs::create_dir_all(root.join("ws")).unwrap();
        std::fs::write(
            root.join("ws/angular.json"),
            r#"{
  "projects": {
    "app": {
      "root": "",
      "architect": {
        "build": {
          "options": {
            "styles": ["ghost.scss", "real.scss"]
          }
        }
      }
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(root.join("ws/real.scss"), "").unwrap();

        let host_tmp = tempfile::tempdir().unwrap();
        materialize(host_tmp.path()).unwrap();
        let sink = LogSink::new(false);
        bridge_global_styles(root, &comp, host_tmp.path(), &sink);

        let bridged: Value = serde_json::from_str(
            &std::fs::read_to_string(host_tmp.path().join("angular.json")).unwrap(),
        )
        .unwrap();
        let styles: Vec<String> = bridged
            .pointer("/projects/host/architect/build/options/styles")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .expect("styles present");
        assert!(
            styles.contains(&"./external/ws/real.scss".to_string()),
            "existing style bridged: {styles:?}"
        );
        assert!(
            !styles.iter().any(|s| s.contains("ghost")),
            "missing style skipped instead of breaking the build: {styles:?}"
        );
        // The skip is visible in the logs, never silent.
        let logs: Vec<String> = sink.range(0, sink.len());
        assert!(
            logs.iter().any(|l| l.contains("ghost.scss") && l.contains("skipped")),
            "skip logged: {logs:?}"
        );
    }

    #[test]
    fn bridge_includes_ambient_declarations_from_tsconfig_include() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        let comp_dir = root.join("gym/projects/app/src/lib");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.component.ts");
        std::fs::write(&comp, "@Component({}) export class X {}").unwrap();

        std::fs::create_dir_all(root.join("gym/src")).unwrap();
        std::fs::write(
            root.join("gym/src/typings.d.ts"),
            "interface String { zfill(n: number): string; }",
        )
        .unwrap();
        std::fs::write(
            root.join("gym/tsconfig.json"),
            r#"{
  "compilerOptions": {
    "baseUrl": ".",
    "include": ["src/**/*.d.ts"]
  }
}"#,
        )
        .unwrap();

        let host_tmp = tempfile::tempdir().unwrap();
        materialize(host_tmp.path()).unwrap();
        let sink = LogSink::new(false);
        bridge_tsconfig_paths(root, &comp, host_tmp.path(), &sink);

        let bridged: Value = serde_json::from_str(
            &std::fs::read_to_string(host_tmp.path().join("tsconfig.json")).unwrap(),
        )
        .unwrap();
        let files = bridged["files"].as_array().expect("files present");
        assert!(
            files.iter().any(|f| f == "./external/gym/src/typings.d.ts"),
            "ambient declarations bridged: {files:?}"
        );
    }

    #[test]
    fn bridge_globs_dts_when_tsconfig_declares_none() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        let comp_dir = root.join("app/src/lib");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.component.ts");
        std::fs::write(&comp, "export const x = 1;").unwrap();

        // No include/files in tsconfig, but a typings file exists under root.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/typings.d.ts"),
            "declare const VERSION: string;",
        )
        .unwrap();
        std::fs::write(root.join("tsconfig.json"), r#"{ "compilerOptions": {} }"#).unwrap();

        let host_tmp = tempfile::tempdir().unwrap();
        materialize(host_tmp.path()).unwrap();
        let sink = LogSink::new(false);
        bridge_tsconfig_paths(root, &comp, host_tmp.path(), &sink);

        let bridged: Value = serde_json::from_str(
            &std::fs::read_to_string(host_tmp.path().join("tsconfig.json")).unwrap(),
        )
        .unwrap();
        let files = bridged["files"].as_array().expect("files present");
        assert!(
            files.iter().any(|f| f == "./external/src/typings.d.ts"),
            "glob fallback picks typings: {files:?}"
        );
        assert!(
            !files
                .iter()
                .any(|f| f.as_str().unwrap_or_default().contains("node_modules")),
            "node_modules excluded from the glob"
        );
    }

    #[test]
    fn bridge_maps_user_paths_and_filters_framework_keys() {
        let root_tmp = tempfile::tempdir().unwrap();
        let root = root_tmp.path();
        // Nested workspace: root/gym/tsconfig.json (extends base) — component sits deeper.
        let comp_dir = root.join("gym/projects/app/src/lib");
        std::fs::create_dir_all(&comp_dir).unwrap();
        let comp = comp_dir.join("x.component.ts");
        std::fs::write(&comp, "@Component({}) export class X {}").unwrap();

        std::fs::write(
            root.join("gym/tsconfig.base.json"),
            r#"{
  "compilerOptions": {
    "baseUrl": ".",
    "strict": false,
    "target": "es2022",
    "paths": {
      "@angular/core": ["node_modules/@angular/core"],
      "@lib/*": ["projects/lib/src/*"],
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("gym/tsconfig.json"),
            r#"{
  // app config extends the base
  "extends": "./tsconfig.base.json",
  "compilerOptions": {}
}"#,
        )
        .unwrap();

        let host_tmp = tempfile::tempdir().unwrap();
        materialize(host_tmp.path()).unwrap();
        let sink = LogSink::new(false);

        bridge_tsconfig_paths(root, &comp, host_tmp.path(), &sink);

        let bridged: Value = serde_json::from_str(
            &std::fs::read_to_string(host_tmp.path().join("tsconfig.json")).unwrap(),
        )
        .unwrap();
        let paths = bridged["compilerOptions"]["paths"]
            .as_object()
            .expect("paths present");
        assert!(
            paths.contains_key("@lib/*"),
            "user paths bridged: {paths:?}"
        );
        assert!(
            !paths.contains_key("@angular/core"),
            "framework keys filtered"
        );
        assert_eq!(
            paths["@lib/*"][0], "./external/gym/projects/lib/src/*",
            "targets re-anchored under the config dir inside external/"
        );
        // Type-checking flags follow the user's workspace, not the host's.
        assert_eq!(bridged["compilerOptions"]["strict"], false);
        assert_eq!(bridged["compilerOptions"]["target"], "es2022");
        // Build plumbing stays ours.
        assert_eq!(bridged["compilerOptions"]["module"], "es2022");
    }

    #[test]
    fn entry_is_stable_for_same_input() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let comp = root.join("a.component.ts");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&comp, "x").unwrap();

        let e1 = write_entry(dir.path(), &comp, &root, &info()).unwrap();
        let c1 = std::fs::read_to_string(&e1).unwrap();
        let e2 = write_entry(dir.path(), &comp, &root, &info()).unwrap();
        let c2 = std::fs::read_to_string(&e2).unwrap();
        assert_eq!(
            c1, c2,
            "same selection must not change entry content (no rebuild)"
        );
    }

    #[test]
    fn stubs_provide_activexmodal_only_for_modal_components() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();

        // Plain component: empty provider list, no ng-bootstrap import.
        let plain = root.join("plain.component.ts");
        std::fs::write(&plain, "export class PlainComponent {}").unwrap();
        write_entry(dir.path(), &plain, &root, &info()).unwrap();
        let stubs = std::fs::read_to_string(dir.path().join("src/selected/stubs.ts")).unwrap();
        assert!(stubs.contains("stubProviders = []"), "got: {stubs}");
        assert!(
            !stubs.contains(": Provider"),
            "empty stubs must not use the Provider type (no import → TS2304): {stubs}"
        );
        assert!(
            !stubs.contains("import"),
            "empty stubs must have no imports: {stubs}"
        );
        assert!(
            !stubs.contains("@ng-bootstrap"),
            "plain component must not import ng-bootstrap: {stubs}"
        );

        // Modal-content component: stub provider for NgbActiveModal.
        let modal = root.join("modal.component.ts");
        std::fs::write(
            &modal,
            "private readonly activeModal = inject(NgbActiveModal);",
        )
        .unwrap();
        write_entry(dir.path(), &modal, &root, &info()).unwrap();
        let stubs = std::fs::read_to_string(dir.path().join("src/selected/stubs.ts")).unwrap();
        assert!(stubs.contains("provide: NgbActiveModal"), "got: {stubs}");
        assert!(stubs.contains("@ng-bootstrap/ng-bootstrap"), "got: {stubs}");
    }
}
