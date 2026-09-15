//! MCP tool registry: every tool is a real operation over the existing
//! domain code (detection, pipeline validation, dev-server lifecycle, logs).
//! Tool failures surface as `isError: true` tool results (MCP contract),
//! never as protocol-level errors — the agent keeps the conversation.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::Session;
use crate::detect;
use crate::explorer::finder;
use crate::pipeline;
use crate::serve;

/// A single tool: name, agent-facing docs, JSON schema and the handler.
struct ToolDef {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

/// The full registry, built per call (`json!` is not const-evaluable).
fn tools() -> Vec<ToolDef> {
    vec![
    ToolDef {
        name: "list_components",
        description: "List renderable UI components under the project root \
                      (Angular components; React/Svelte/Vue are classified but not \
                      renderable yet). Optionally filter by a path substring. \
                      Paginated: returns count/returned/nextOffset.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "filter": { "type": "string", "description": "Optional path substring filter." },
                "limit": { "type": "integer", "description": "Max components per page (default 100)." },
                "offset": { "type": "integer", "description": "Skip the first N matches (default 0)." }
            }
        }),
    },
    ToolDef {
        name: "inspect_component",
        description: "Validate a component and return its render metadata: class name, \
                      mount strategy (standalone/module), declaring NgModule, required \
                      signal inputs and the fictional typed input values used at mount.",
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "description": "Component file path (absolute, or relative to the project root)." }
            }
        }),
    },
    ToolDef {
        name: "select_component",
        description: "Render a component in the browser: starts the dev server on a free \
                      port the first time, then hot-swaps subsequent selections without \
                      restarting. Cold start can take minutes on big workspaces, so this \
                      answers immediately with {\"starting\": true}; poll get_status \
                      until running=true to get the URL.",
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "description": "Component file path (absolute, or relative to the project root)." }
            }
        }),
    },
    ToolDef {
        name: "get_status",
        description: "Current session state: whether the dev server is running or still \
                      starting, its URL and ports, which component is being served, and \
                      the last start error if any.",
        input_schema: json!({ "type": "object", "properties": {} }),
    },
    ToolDef {
        name: "get_logs",
        description: "Tail the shared log stream: npm installs, ng serve output, build \
                      settlements and errors.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "tail": { "type": "integer", "description": "Number of recent lines (default 100)." }
            }
        }),
    },
    ToolDef {
        name: "stop_server",
        description: "Stop the dev server started by this session (graceful).",
        input_schema: json!({ "type": "object", "properties": {} }),
    },
    ToolDef {
        name: "open_in_browser",
        description: "Open the running preview URL in the user's default browser.",
        input_schema: json!({ "type": "object", "properties": {} }),
    },
    ToolDef {
        name: "find_files",
        description: "Fuzzy-find files under the project root (fzf-like smart-case \
                      subsequence scoring, basename matches rank higher).",
        input_schema: json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string", "description": "Fuzzy query, e.g. 'pdfview'." },
                "limit": { "type": "integer", "description": "Max matches to return (default 20)." }
            }
        }),
    },
    ToolDef {
        name: "read_file",
        description: "Read a text file inside the project root (paths outside the root \
                      are rejected). Truncated at 256 KiB.",
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "description": "File path (absolute, or relative to the project root)." }
            }
        }),
    },
    ]
}

/// `tools/list` payload.
pub fn registry() -> Vec<Value> {
    tools()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect()
}

/// `tools/call` dispatcher. Returns the MCP tool result envelope.
pub async fn call(session: &mut Session, params: &Value) -> Result<Value, super::RpcError> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| super::rpc_error(super::INVALID_PARAMS, "missing tool name".into()))?;
    if !tools().iter().any(|t| t.name == name) {
        return Err(super::rpc_error(
            super::INVALID_PARAMS,
            format!("unknown tool: {name}"),
        ));
    }
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match name {
        "list_components" => {
            let filter = args.get("filter").and_then(|f| f.as_str());
            let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(100);
            let offset = args.get("offset").and_then(|o| o.as_u64()).unwrap_or(0);
            Ok(components_json(
                session.root(),
                filter,
                limit.min(10_000) as usize,
                offset as usize,
            ))
        }
        "inspect_component" => inspect(session, &args),
        "select_component" => select(session, &args).await,
        "get_status" => {
            super::harvest_start(session).await;
            Ok(status_json(session))
        }
        "get_logs" => {
            let tail = args.get("tail").and_then(|t| t.as_u64()).unwrap_or(100) as usize;
            Ok(json!({ "lines": session.sink.tail(tail) }))
        }
        "stop_server" => stop(session).await,
        "open_in_browser" => open_browser(session),
        "find_files" => find_files(session, &args),
        "read_file" => read_file(session, &args),
        _ => unreachable!("name checked above"),
    };

    Ok(match result {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "isError": false
        }),
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true
        }),
    })
}

// ---------------------------------------------------------------------------
// Shared JSON builders (tools + resources)
// ---------------------------------------------------------------------------

/// Component index under the project root, classified by kind.
/// `limit` of 0 means no limit (used by the `rc://components` resource).
pub fn components_json(root: &Path, filter: Option<&str>, limit: usize, offset: usize) -> Value {
    let items = finder::index_files(root);
    let mut components: Vec<Value> = Vec::new();
    for item in items {
        if !item.display.ends_with(".ts") && !item.display.ends_with(".tsx") {
            continue;
        }
        if let Some(filter) = filter {
            if !item.display.contains(filter) {
                continue;
            }
        }
        let kind = detect::classify(&item.path);
        if !kind.highlighted() {
            continue;
        }
        components.push(json!({
            "path": item.display,
            "kind": kind.as_str(),
            "renderable": kind.renderable(),
        }));
    }
    let total = components.len();
    let page: Vec<Value> = if limit == 0 {
        components.into_iter().skip(offset).collect()
    } else {
        components.into_iter().skip(offset).take(limit).collect()
    };
    let mut out = json!({
        "count": total,
        "offset": offset,
        "returned": page.len(),
        "components": page,
    });
    if limit > 0 && offset + page.len() < total {
        out["nextOffset"] = json!(offset + page.len());
        out["detail"] = json!("truncated: pass limit/offset for pages, or a filter to narrow");
    }
    out
}

/// Live session status (tool + resource payload).
pub fn status_json(session: &Session) -> Value {
    json!({
        "running": session.server.is_some(),
        "starting": session.start_task.is_some(),
        "startError": session.start_error,
        "url": session.server.as_ref().map(|h| h.vite_url.clone()),
        "cliPort": session.server.as_ref().map(|h| h.cli_port),
        "vitePort": session.server.as_ref().map(|h| h.vite_port),
        "current": session.current.as_ref().map(|c| c.display().to_string()),
        "startingComponent": session.starting.as_ref().map(|c| c.display().to_string()),
        "root": session.root.display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Resolve a tool path argument (absolute or root-relative) and reject escapes.
fn resolve_in_root(session: &Session, raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        session.root().join(path)
    };
    let canonical = absolute.canonicalize().map_err(|e| {
        format!("not found: {raw} ({e})")
    })?;
    if !canonical.starts_with(session.root()) {
        return Err(format!(
            "path escapes the project root: {raw}"
        ));
    }
    Ok(canonical)
}

fn required(session: &Session, args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!("missing required argument: {key}"))
        .and_then(|raw| resolve_in_root(session, &raw).map(|p| p.display().to_string()))
}

fn inspect(session: &Session, args: &Value) -> Result<Value, String> {
    let path_str = required(session, args, "path")?;
    let path = PathBuf::from(&path_str);
    let info = pipeline::validate(&path, session.root()).map_err(|e| e.to_string())?;
    Ok(json!({
        "path": path_str,
        "class_name": info.class_name,
        "strategy": info.strategy.as_str(),
        "module": info.module_class_name,
        "required_inputs": info.required_inputs,
        "input_values": info.input_values,
    }))
}

async fn select(session: &mut Session, args: &Value) -> Result<Value, String> {
    let path_str = required(session, args, "path")?;
    let path = PathBuf::from(&path_str);
    // Validate first so a broken component never spins up tooling.
    let info = pipeline::validate(&path, session.root()).map_err(|e| e.to_string())?;

    // Promote a just-finished cold start before deciding the route.
    super::harvest_start(session).await;

    if let Some(handle) = session.server.as_ref() {
        // Hot-swap path (R7): selection file + revision bump, no restart.
        serve::write_selection_file(&selection_file_for(), &path, session.root())
            .map_err(|e| format!("cannot write selection file: {e}"))?;
        session.current = Some(path);
        session.sink.push(format!("[mcp] hot-swap -> {path_str}"));
        return Ok(json!({
            "url": handle.vite_url,
            "component": path_str,
            "strategy": info.strategy.as_str(),
            "hot_swap": true,
        }));
    }

    // Cold start already in flight: report progress instead of blocking past
    // the client's tool timeout (which is how duplicate servers used to spawn).
    if session.start_task.is_some() {
        return Ok(json!({
            "starting": true,
            "component": path_str,
            "url": Value::Null,
            "detail": "dev server is still starting; poll get_status until running=true"
        }));
    }

    let opts = serve::ServeOptions {
        component: path.clone(),
        root: session.root().to_path_buf(),
        port: None,
        selection_file: selection_file_for(),
        open_browser: false,
        sink: session.sink.clone(),
    };
    session.start_error = None;
    session.starting = Some(path.clone());
    session.start_task = Some(tokio::spawn(async move { serve::start(opts).await }));
    session.sink.push(format!("[mcp] starting dev server for {path_str}..."));
    Ok(json!({
        "starting": true,
        "component": path_str,
        "url": Value::Null,
        "strategy": info.strategy.as_str(),
        "detail": "cold start launched; poll get_status until running=true"
    }))
}

/// Session-private selection IPC file so an MCP session never clobbers the
/// TUI's selection stream.
fn selection_file_for() -> PathBuf {
    std::env::temp_dir().join("render-component-mcp-selection.json")
}

async fn stop(session: &mut Session) -> Result<Value, String> {
    // A cold start still in flight: cancel it outright.
    if let Some(task) = session.start_task.take() {
        task.abort();
        session.starting = None;
        session.sink.push("[mcp] cold start cancelled");
        return Ok(json!({ "stopped": true, "reason": "start cancelled" }));
    }
    match session.server.take() {
        Some(handle) => {
            let url = handle.vite_url.clone();
            handle.shutdown().await;
            session.current = None;
            session.sink.push("[mcp] dev server stopped");
            Ok(json!({ "stopped": true, "url": url }))
        }
        None => Ok(json!({ "stopped": false, "reason": "server not running" })),
    }
}

fn open_browser(session: &mut Session) -> Result<Value, String> {
    let Some(handle) = session.server.as_ref() else {
        return Err("server not running — select_component first".into());
    };
    crate::host::open_browser(&handle.vite_url);
    session.sink.push(format!("[mcp] opened {}", handle.vite_url));
    Ok(json!({ "opened": handle.vite_url }))
}

fn find_files(session: &Session, args: &Value) -> Result<Value, String> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .ok_or_else(|| "missing required argument: query".to_string())?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(20) as usize;

    let items = finder::index_files(session.root());
    let mut scored: Vec<(i64, &str)> = items
        .iter()
        .filter_map(|item| finder::score(query, &item.display).map(|(s, _)| (s, item.display.as_str())))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let matches: Vec<Value> = scored
        .into_iter()
        .take(limit)
        .map(|(score, path)| json!({ "path": path, "score": score }))
        .collect();
    Ok(json!({ "count": matches.len(), "matches": matches }))
}

const MAX_READ_BYTES: u64 = 256 * 1024;

fn read_file(session: &Session, args: &Value) -> Result<Value, String> {
    let path_str = required(session, args, "path")?;
    let path = PathBuf::from(&path_str);
    let meta = std::fs::metadata(&path).map_err(|e| format!("cannot stat {path_str}: {e}"))?;
    let truncated = meta.len() > MAX_READ_BYTES;
    let file = std::fs::File::open(&path).map_err(|e| format!("cannot read {path_str}: {e}"))?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = String::new();
    std::io::Read::take(&mut reader, MAX_READ_BYTES)
        .read_to_string(&mut buf)
        .map_err(|e| format!("cannot read {path_str}: {e}"))?;
    Ok(json!({
        "path": path_str,
        "bytes": meta.len(),
        "truncated": truncated,
        "content": buf,
    }))
}
