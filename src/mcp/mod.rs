//! MCP (Model Context Protocol) server over stdio — lets AI agents operate
//! render-component programmatically: discover components, inspect their
//! metadata, start/hot-swap the dev server, tail logs and shut down cleanly.
//!
//! Transport: newline-delimited JSON-RPC 2.0 (the MCP stdio transport).
//! The only bytes on stdout are protocol responses; every log line goes to
//! the shared `LogSink` (exposed as an MCP resource), never to stdout.
//!
//! Protocol surface (MCP `2025-06-18`, negotiating down to `2024-11-05`):
//! - `initialize`, `ping`, `logging/setLevel`
//! - `tools/list`, `tools/call`          (see `tools.rs` for the registry)
//! - `resources/list`, `resources/read`  (status, logs, component index)
//! - `prompts/list`, `prompts/get`       (agent playbooks)

mod tools;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::logs::LogSink;
use crate::serve::ServeHandle;

/// Protocol versions this server can speak. We answer with the client's
/// requested version when we support it, otherwise our newest.
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_VERSION: &str = "2025-06-18";

const SERVER_NAME: &str = "render-component";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

const SERVER_INSTRUCTIONS: &str = "\
render-component renders a single Angular component in the browser.
Typical loop: list_components → inspect_component → select_component \
(returns the dev-server URL) → get_logs / get_status → stop_server. \
Use find_files to locate files by fuzzy name and read_file to view \
any file inside the project root. \
Cold start compiles the whole workspace and can take minutes: \
select_component may answer {\"starting\": true} instead of blocking — \
poll get_status until running=true and the URL appears.";

/// JSON-RPC error codes used here.
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// One MCP client session: project root plus the optional running dev server.
pub struct Session {
    root: PathBuf,
    sink: LogSink,
    server: Option<ServeHandle>,
    /// Component currently being served (absolute path), if any.
    current: Option<PathBuf>,
    /// In-flight cold start, if any. The tool answers immediately and agents
    /// poll `get_status` instead of blocking past client timeouts.
    start_task: Option<tokio::task::JoinHandle<anyhow::Result<ServeHandle>>>,
    /// Component the in-flight cold start was launched for.
    starting: Option<PathBuf>,
    /// Error of the last failed cold start, surfaced via `get_status`.
    start_error: Option<String>,
}

impl Session {
    pub fn new(root: PathBuf) -> Session {
        Session {
            root,
            sink: LogSink::new(false),
            server: None,
            current: None,
            start_task: None,
            starting: None,
            start_error: None,
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

/// Process one raw stdin line. Returns the response line (already JSON),
/// or None for notifications/malformed-no-id cases that must not answer.
pub async fn handle_line(session: &mut Session, line: &str) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return Some(error_response(Value::Null, PARSE_ERROR, "parse error: invalid JSON"));
        }
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();

    // Notifications (no id): acknowledged silently per the JSON-RPC rules.
    if !msg.get("id").is_some() {
        return None;
    }

    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "logging/setLevel" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::registry() })),
        "tools/call" => tools::call(session, &params).await,
        "resources/list" => Ok(resources_list(session)),
        "resources/read" => resources_read(session, &params),
        "prompts/list" => Ok(prompts_list()),
        "prompts/get" => prompts_get(&params),
        other => Err(rpc_error(
            METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        )),
    };

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }).to_string(),
        Err(err) => error_response(id, err.code, &err.message),
    })
}

fn rpc_error(code: i64, message: String) -> RpcError {
    RpcError { code, message }
}

struct RpcError {
    code: i64,
    message: String,
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

fn initialize_result(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(|v| v.as_str());
    let version = requested
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(LATEST_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
            "prompts": { "listChanged": false },
            "logging": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
        },
        "instructions": SERVER_INSTRUCTIONS
    })
}

// ---------------------------------------------------------------------------
// Resources: live JSON views over session state. Read on demand, never cached.
// ---------------------------------------------------------------------------

const URI_STATUS: &str = "rc://status";
const URI_LOGS: &str = "rc://logs";
const URI_COMPONENTS: &str = "rc://components";

fn resources_list(_session: &Session) -> Value {
    json!({
        "resources": [
            {
                "uri": URI_STATUS,
                "name": "Server status",
                "description": "Dev-server state: running, URL, current component.",
                "mimeType": "application/json"
            },
            {
                "uri": URI_LOGS,
                "name": "Server logs",
                "description": "Everything streamed so far: npm, ng serve, TUI notes.",
                "mimeType": "application/json"
            },
            {
                "uri": URI_COMPONENTS,
                "name": "Component index",
                "description": "Every renderable component found under the project root.",
                "mimeType": "application/json"
            }
        ]
    })
}

fn resources_read(session: &mut Session, params: &Value) -> Result<Value, RpcError> {
    let uri = params
        .get("uri")
        .and_then(|u| u.as_str())
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "missing resource uri".into()))?;
    let text = match uri {
        URI_STATUS => tools::status_json(session).to_string(),
        URI_LOGS => json!({ "lines": session.sink.tail(500) }).to_string(),
        URI_COMPONENTS => tools::components_json(session.root(), None, 0, 0).to_string(),
        other => {
            return Err(rpc_error(
                INVALID_PARAMS,
                format!("unknown resource: {other}"),
            ));
        }
    };
    Ok(json!({
        "contents": [{ "uri": uri, "mimeType": "application/json", "text": text }]
    }))
}

// ---------------------------------------------------------------------------
// Prompts: reusable agent playbooks.
// ---------------------------------------------------------------------------

fn prompts_list() -> Value {
    json!({
        "prompts": [
            {
                "name": "select-component",
                "description": "Find and render a component by fuzzy name.",
                "arguments": [
                    {
                        "name": "query",
                        "description": "Fuzzy name, e.g. 'pdfview' or 'gallery'.",
                        "required": true
                    }
                ]
            },
            {
                "name": "diagnose-render",
                "description": "Diagnose why a component is not rendering as expected.",
                "arguments": [
                    {
                        "name": "component",
                        "description": "Component path currently being served.",
                        "required": true
                    }
                ]
            }
        ]
    })
}

fn prompts_get(params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "missing prompt name".into()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let arg = |key: &str| {
        args.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let user_text = match name {
        "select-component" => {
            let query = arg("query");
            format!(
                "Use find_files with query '{query}', inspect the best match with \
                 inspect_component, then select_component it. Report the dev-server URL, \
                 the mount strategy and any required inputs."
            )
        }
        "diagnose-render" => {
            let component = arg("component");
            format!(
                "The user reports '{component}' is not rendering as expected. Check \
                 get_status and get_logs for build errors, inspect_component for the \
                 mount strategy and required inputs, and read_file the source if needed. \
                 Report the root cause and the exact next action."
            )
        }
        other => return Err(rpc_error(METHOD_NOT_FOUND, format!("unknown prompt: {other}"))),
    };
    Ok(json!({
        "description": format!("render-component: {name}"),
        "messages": [
            {
                "role": "user",
                "content": { "type": "text", "text": user_text }
            }
        ]
    }))
}

// ---------------------------------------------------------------------------
// stdio entrypoint
// ---------------------------------------------------------------------------

/// Run the MCP server over stdin/stdout until EOF. On exit, the dev server
/// (if the session started one) is shut down gracefully.
pub async fn run(root: PathBuf) -> anyhow::Result<()> {
    let lockfile = lockfile_for(&root);
    takeover_stale_instance(&lockfile);
    claim_lockfile(&lockfile, None);
    let mut session = Session::new(root);
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&mut session, &line).await {
            stdout.write_all(response.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    if let Some(task) = session.start_task.take() {
        task.abort();
    }
    if let Some(handle) = session.server.take() {
        handle.shutdown().await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Orphan takeover: a client that times out or is killed mid-start leaves the
// MCP process — and its `ng serve` child — running forever. A fresh instance
// asks the orphan's control API to shut down gracefully first (so the child
// dies with it), then SIGKILLs whatever remains.
// ---------------------------------------------------------------------------

/// Per-root lockfile so simultaneous MCP instances serving different projects
/// never kill each other.
fn lockfile_for(root: &Path) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.display().to_string().bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    std::env::temp_dir().join(format!("render-component-mcp-{hash:016x}.json"))
}

fn process_cmdline(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|c| c.replace('\0', " "))
}

/// Best-effort: POST /api/shutdown with a short timeout so the orphan's
/// control API kills its `ng serve` child before we SIGKILL the process.
fn request_graceful_shutdown(cli_port: u16) {
    use std::io::Write as _;
    use std::net::TcpStream;
    use std::time::Duration;

    let addr = format!("127.0.0.1:{cli_port}");
    if let Ok(mut stream) = TcpStream::connect_timeout(
        &addr.parse().expect("static addr"),
        Duration::from_secs(2),
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let req = "POST /api/shutdown HTTP/1.0\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n";
        let _ = stream.write_all(req.as_bytes());
        let _ = stream.flush();
    }
}

fn takeover_stale_instance(lockfile: &Path) {
    let record = |v: &Value| v["pid"].as_u64();
    let stale = std::fs::read_to_string(lockfile)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(|v| {
            record(v)
                .map(|pid| pid != std::process::id() as u64)
                .unwrap_or(false)
        });
    let Some(v) = stale else {
        return;
    };
    let pid = record(&v).expect("filtered above") as u32;
    if process_cmdline(pid).is_some_and(|c| c.contains("render-component")) {
        if let Some(port) = v["cli_port"].as_u64() {
            request_graceful_shutdown(port as u16);
        }
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(lockfile);
}

/// (Re)write this instance's lockfile record.
fn claim_lockfile(lockfile: &Path, cli_port: Option<u16>) {
    let payload = json!({
        "pid": std::process::id(),
        "cli_port": cli_port,
    });
    let _ = std::fs::write(lockfile, payload.to_string());
}

/// Harvest a finished cold-start task: promote success into the live server,
/// record failure into `start_error`. Instant when the task already finished.
async fn harvest_start(session: &mut Session) {
    let Some(task) = session.start_task.as_ref() else {
        return;
    };
    if !task.is_finished() {
        return;
    }
    let task = session.start_task.take().expect("checked above");
    match task.await {
        Ok(Ok(handle)) => {
            let url = handle.vite_url.clone();
            let cli_port = handle.cli_port;
            session.server = Some(handle);
            if let Some(component) = session.starting.take() {
                session.current = Some(component);
            }
            session.start_error = None;
            session.sink.push(format!("[mcp] dev server ready at {url}"));
            claim_lockfile(&lockfile_for(&session.root), Some(cli_port));
        }
        Ok(Err(e)) => {
            session.starting = None;
            session.start_error = Some(e.to_string());
            session.sink.push(format!("[mcp!] dev server failed: {e:#}"));
        }
        Err(e) => {
            session.starting = None;
            session.start_error = Some(format!("start task panicked: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn call(session: &mut Session, request: Value) -> Value {
        let raw = handle_line(session, &request.to_string())
            .await
            .expect("request must answer");
        serde_json::from_str(&raw).expect("response must be valid JSON")
    }

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-angular")
    }

    #[tokio::test]
    async fn initialize_negotiates_supported_versions() {
        let mut session = Session::new(fixture_root());
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
        )
        .await;
        let result = &res["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05", "echo a version we support");
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["tools"].is_object());

        // Unsupported request falls back to our latest.
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}),
        )
        .await;
        assert_eq!(res["result"]["protocolVersion"], LATEST_VERSION);
    }

    #[tokio::test]
    async fn notifications_and_bad_json_follow_jsonrpc_rules() {
        let mut session = Session::new(fixture_root());
        // Notification (no id): never answered.
        assert!(handle_line(
            &mut session,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .await
        .is_none());
        // Malformed JSON: parse error with null id.
        let raw = handle_line(&mut session, "{not json").await.unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["error"]["code"], PARSE_ERROR);
        assert_eq!(v["id"], Value::Null);
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let mut session = Session::new(fixture_root());
        let res = call(&mut session, json!({"jsonrpc":"2.0","id":7,"method":"wat/xyz"})).await;
        assert_eq!(res["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_list_exposes_the_full_registry_with_schemas() {
        let mut session = Session::new(fixture_root());
        let res = call(&mut session, json!({"jsonrpc":"2.0","id":3,"method":"tools/list"})).await;
        let tools = res["result"]["tools"].as_array().expect("tools array");
        assert!(tools.len() >= 9, "extensive registry, got {}", tools.len());
        for tool in tools {
            assert!(tool["name"].is_string(), "every tool has a name");
            assert_eq!(tool["inputSchema"]["type"], "object", "schema per tool");
            assert!(tool["description"].is_string());
        }
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        for expected in [
            "list_components",
            "inspect_component",
            "select_component",
            "get_status",
            "get_logs",
            "stop_server",
            "open_in_browser",
            "find_files",
            "read_file",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[tokio::test]
    async fn inspect_component_reports_metadata_for_a_fixture() {
        let mut session = Session::new(fixture_root());
        let comp = fixture_root().join("gallery/gallery.component.ts");
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"inspect_component",
                "arguments":{"path": comp.to_string_lossy()}
            }}),
        )
        .await;
        let content = res["result"]["content"][0]["text"].as_str().expect("text content");
        let info: Value = serde_json::from_str(content).unwrap();
        assert_eq!(info["class_name"], "GalleryComponent");
        assert_eq!(info["strategy"], "standalone");
        assert!(res["result"]["isError"].is_null() || res["result"]["isError"] == false);
    }

    #[tokio::test]
    async fn tool_failures_surface_as_iserror_not_protocol_errors() {
        let mut session = Session::new(fixture_root());
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
                "name":"inspect_component",
                "arguments":{"path": "/definitely/missing.component.ts"}
            }}),
        )
        .await;
        assert_eq!(res["result"]["isError"], true, "tool failure is a tool result");
        assert!(res["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not found"));
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let mut session = Session::new(fixture_root());
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{
                "name":"nope","arguments":{}
            }}),
        )
        .await;
        assert_eq!(res["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn find_files_ranks_the_gallery_fixture_first() {
        let mut session = Session::new(fixture_root());
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{
                "name":"find_files","arguments":{"query":"gal"}
            }}),
        )
        .await;
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        let found: Value = serde_json::from_str(text).unwrap();
        let first = found["matches"][0]["path"].as_str().unwrap();
        assert!(first.starts_with("gallery/"), "best match under gallery/: {found}");
        let paths: Vec<&str> = found["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["path"].as_str())
            .collect();
        assert!(
            paths.contains(&"gallery/gallery.component.ts"),
            "the component file is among the matches: {found}"
        );
        // Scores come back descending.
        let scores: Vec<i64> = found["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["score"].as_i64())
            .collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted, "matches sorted by score desc: {found}");
    }

    #[tokio::test]
    async fn read_file_is_confined_to_the_project_root() {
        let mut session = Session::new(fixture_root());
        // Inside the root: works.
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{
                "name":"read_file","arguments":{"path":"plain/helper.ts"}
            }}),
        )
        .await;
        let text = res["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("content"), "returns file payload: {text}");

        // Outside the root: tool error, never a traversal.
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
                "name":"read_file","arguments":{"path":"../../Cargo.toml"}
            }}),
        )
        .await;
        assert_eq!(res["result"]["isError"], true, "path escape is rejected");
    }

    #[tokio::test]
    async fn resources_round_trip() {
        let mut session = Session::new(fixture_root());
        let res = call(&mut session, json!({"jsonrpc":"2.0","id":11,"method":"resources/list"})).await;
        let uris: Vec<&str> = res["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["uri"].as_str())
            .collect();
        assert!(uris.contains(&URI_STATUS) && uris.contains(&URI_LOGS));

        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":12,"method":"resources/read","params":{"uri":URI_LOGS}}),
        )
        .await;
        let text = res["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(serde_json::from_str::<Value>(text).is_ok(), "logs payload is JSON");
    }

    #[tokio::test]
    async fn prompts_list_and_get() {
        let mut session = Session::new(fixture_root());
        let res = call(&mut session, json!({"jsonrpc":"2.0","id":13,"method":"prompts/list"})).await;
        assert_eq!(res["result"]["prompts"].as_array().unwrap().len(), 2);

        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":14,"method":"prompts/get","params":{
                "name":"select-component","arguments":{"query":"pdfview"}
            }}),
        )
        .await;
        let text = res["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("pdfview"), "argument lands in the prompt");
    }

    #[tokio::test]
    async fn ping_and_set_level_answer_empty_results() {
        let mut session = Session::new(fixture_root());
        let res = call(&mut session, json!({"jsonrpc":"2.0","id":15,"method":"ping"})).await;
        assert_eq!(res["result"], json!({}));
        let res = call(
            &mut session,
            json!({"jsonrpc":"2.0","id":16,"method":"logging/setLevel","params":{"level":"debug"}}),
        )
        .await;
        assert_eq!(res["result"], json!({}));
    }
}
