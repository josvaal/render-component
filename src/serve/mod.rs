//! Control server: free-port binding, `/api/current`, SSE `/api/events`,
//! `POST /api/select`, selection-file watcher and clean shutdown (C04, C07, C11, C13).

mod state;

pub use crate::logs::LogSink;
pub use state::{
    AppState, CurrentSelection, SelectOutcome, read_selection_file, write_selection_file,
};

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive};
use axum::response::{Redirect, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use tokio_stream::wrappers::WatchStream;

use crate::cli;
use crate::host;
use crate::pipeline;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub component: PathBuf,
    pub root: PathBuf,
    pub port: Option<u16>,
    pub selection_file: PathBuf,
    pub open_browser: bool,
    pub sink: LogSink,
}

/// Everything the handlers and the watcher need. `host_dir` is required by
/// `write_entry`; in tests it points at a throwaway dir.
pub struct Ctx {
    pub app: AppState,
    pub vite_port: u16,
    pub host_dir: PathBuf,
    pub sink: LogSink,
    /// True between a selection change and the dev-server build settling.
    pub build_pending: Arc<std::sync::atomic::AtomicBool>,
    /// True when the dev-server process ended unexpectedly.
    pub ng_exited: Arc<std::sync::atomic::AtomicBool>,
    /// Shared shutdown trigger: `POST /api/shutdown` and `ServeHandle::shutdown`
    /// both flip it; the axum graceful-shutdown future watches it.
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
}

pub struct ServeHandle {
    pub cli_port: u16,
    #[allow(dead_code)] // part of the handle's public surface for embedders
    pub vite_port: u16,
    pub vite_url: String,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    server_task: tokio::task::JoinHandle<()>,
    /// Owns the `ng serve` child: reacts to the shared shutdown trigger.
    supervisor_task: tokio::task::JoinHandle<()>,
}

/// Reserve a free TCP port by binding and reporting it (C04). The listener is
/// returned still bound so the port stays reserved until the server starts.
pub fn bind_free_port() -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", 0))
}

fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/api/current", get(current))
        .route("/api/ready", get(ready))
        .route("/api/events", get(events))
        .route(
            "/api/select",
            get(select_component_get).post(select_component),
        )
        .route("/api/shutdown", post(shutdown_now))
        .route("/", get(redirect_root))
        .with_state(ctx)
}

async fn current(State(ctx): State<Arc<Ctx>>) -> Json<CurrentSelection> {
    Json(ctx.app.current().await)
}

/// Page-side hot-swap coordination (C31): the page polls this before
/// reloading so it never fetches a bundle mid-write.
async fn ready(State(ctx): State<Arc<Ctx>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "pending": ctx.build_pending.load(std::sync::atomic::Ordering::Relaxed),
        "ngExited": ctx.ng_exited.load(std::sync::atomic::Ordering::Relaxed),
    }))
}

async fn events(State(ctx): State<Arc<Ctx>>) -> impl axum::response::IntoResponse {
    let stream = WatchStream::new(ctx.app.subscribe())
        .map(|revision| {
            Ok::<_, std::convert::Infallible>(Event::default().data(revision.to_string()))
        })
        .boxed();
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(serde::Deserialize)]
struct SelectBody {
    component: String,
}

async fn select_component(
    State(ctx): State<Arc<Ctx>>,
    Json(body): Json<SelectBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    handle_select(&ctx, body.component).await
}

/// GET variant (`/api/select?component=<abs path>`): same handler, same
/// validation — exists so scripts and E2E runners can trigger a hot-swap
/// with a plain navigation (the TUI uses the selection file instead).
#[derive(serde::Deserialize)]
struct SelectQuery {
    component: String,
}

async fn select_component_get(
    State(ctx): State<Arc<Ctx>>,
    axum::extract::Query(q): axum::extract::Query<SelectQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    handle_select(&ctx, q.component).await
}

async fn handle_select(
    ctx: &Arc<Ctx>,
    component: String,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let component = PathBuf::from(&component);
    let component = if component.is_absolute() {
        component
    } else {
        let current = ctx.app.current().await;
        PathBuf::from(&current.root).join(component)
    };
    let root = PathBuf::from(ctx.app.current().await.root);
    match apply_selection(&ctx, &component, &root).await {
        Ok(SelectOutcome::Updated(rev)) => Ok(Json(serde_json::json!({
            "status": "updated", "revision": rev,
        }))),
        Ok(SelectOutcome::Unchanged) => Ok(Json(serde_json::json!({ "status": "unchanged" }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "rejected", "error": e.to_string() })),
        )),
    }
}

async fn redirect_root(State(ctx): State<Arc<Ctx>>) -> Redirect {
    Redirect::temporary(&format!("http://127.0.0.1:{}", ctx.vite_port))
}

/// Remote shutdown (orphan takeover): flips the shared shutdown trigger so the
/// control API stops gracefully and the `ng serve` child is killed.
async fn shutdown_now(State(ctx): State<Arc<Ctx>>) -> Json<serde_json::Value> {
    ctx.sink.push("[serve] shutdown requested via API");
    let _ = ctx.shutdown_tx.send(true);
    Json(serde_json::json!({ "shutting_down": true }))
}

/// Validate + apply a selection; on change, regenerate the host entry so the
/// dev server recompiles (C07). Invalid components are rejected without
/// touching the current state (C10).
pub async fn apply_selection(
    ctx: &Ctx,
    component: &Path,
    root: &Path,
) -> anyhow::Result<SelectOutcome> {
    let info = pipeline::validate(component, root).map_err(|e| anyhow::anyhow!("{e}"))?;
    let outcome = ctx
        .app
        .select(
            component,
            root,
            info.strategy.clone(),
            info.input_values.clone(),
        )
        .await;
    match outcome {
        SelectOutcome::Updated(rev) => {
            host::write_entry(&ctx.host_dir, component, root, &info)
                .context("writing host entry")?;
            ctx.build_pending
                .store(true, std::sync::atomic::Ordering::Relaxed);
            ctx.sink.push(format!(
                "[serve] hot-swap → {} (revision {rev})",
                component.display()
            ));
        }
        SelectOutcome::Unchanged => {
            ctx.sink
                .push(format!("[serve] unchanged: {}", component.display()));
        }
    }
    Ok(outcome)
}

/// Poll the selection IPC file and apply changes (single consumer → last write
/// wins under a rapid race, C11).
fn spawn_watcher(ctx: Arc<Ctx>, selection_file: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let initial = ctx.app.current().await;
        let mut last_seen = Some((initial.component.clone(), initial.root.clone()));
        loop {
            tick.tick().await;
            let Some(sel) = read_selection_file(&selection_file) else {
                continue;
            };
            let component = sel
                .component
                .canonicalize()
                .unwrap_or(sel.component.clone());
            let root = sel.root.canonicalize().unwrap_or(sel.root.clone());
            let key = Some((component.display().to_string(), root.display().to_string()));
            if key == last_seen {
                continue;
            }
            ctx.sink.push(format!(
                "[watcher] selection change → {}",
                component.display()
            ));
            match apply_selection(&ctx, &component, &root).await {
                Ok(SelectOutcome::Updated(_)) | Ok(SelectOutcome::Unchanged) => {
                    last_seen = key;
                }
                Err(e) => {
                    ctx.sink
                        .push(format!("[watcher] rejected {}: {e}", component.display()));
                    ctx.app.report_error(format!("{e}")).await;
                    last_seen = key;
                }
            }
        }
    })
}

/// Bind + run the axum server on an already-bound listener. Returns the shutdown
/// trigger and the server task (C13: graceful shutdown frees the port).
fn spawn_server(
    listener: TcpListener,
    ctx: Arc<Ctx>,
) -> anyhow::Result<(
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
)> {
    let shutdown_rx = ctx.shutdown_tx.subscribe();
    let listener = {
        // std TcpListener::set_nonblocking takes &self.
        listener
            .set_nonblocking(true)
            .context("setting listener non-blocking")?;
        listener
    };
    let tk_listener =
        tokio::net::TcpListener::from_std(listener).context("converting listener to tokio")?;
    let shutdown_tx = ctx.shutdown_tx.clone();
    let server = axum::serve(tk_listener, router(ctx)).with_graceful_shutdown(async move {
        let mut rx = shutdown_rx;
        let _ = rx.changed().await;
    });
    let task = tokio::spawn(async move {
        let _ = server.await;
    });
    Ok((shutdown_tx, task))
}

/// Full orchestration: validate → bind → host up → server + watcher → browser.
pub async fn start(opts: ServeOptions) -> anyhow::Result<ServeHandle> {
    let root = cli::validate_root(&opts.root)?;
    let component = cli::validate_component(&opts.component)?;
    let info = pipeline::validate(&component, &root).map_err(|e| anyhow::anyhow!("{e}"))?;

    let listener = match opts.port {
        Some(p) => TcpListener::bind(("127.0.0.1", p))
            .with_context(|| format!("port {p} is not available"))?,
        None => bind_free_port().context("no free port available")?,
    };
    let cli_port = listener.local_addr()?.port();

    let build_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ng_exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
    let mut ctx = Arc::new(Ctx {
        app: AppState::new(
            &component,
            &root,
            info.strategy.clone(),
            info.required_inputs.clone(),
            info.input_values.clone(),
        ),
        vite_port: 0,
        host_dir: PathBuf::new(),
        sink: opts.sink.clone(),
        build_pending: build_pending.clone(),
        ng_exited: ng_exited.clone(),
        shutdown_tx,
    });
    ctx.sink
        .push(format!("[serve] project root: {}", root.display()));
    ctx.sink
        .push(format!("[serve] component: {}", component.display()));

    let host_dir = host::prepare_host(&root, &component, cli_port, &opts.sink).await?;
    // Entry must exist before the dev server builds, or the first compile fails.
    host::write_entry(&host_dir, &component, &root, &info)?;
    let host_ctx = host::spawn_devserver(
        &host_dir,
        &opts.sink,
        build_pending.clone(),
        ng_exited.clone(),
    )
    .await?;

    // Fill in the values that depend on the dev server.
    {
        let ctx_mut = Arc::get_mut(&mut ctx).expect("no clones handed out yet");
        ctx_mut.vite_port = host_ctx.vite_port;
        ctx_mut.host_dir = host_ctx.dir.clone();
    }

    write_selection_file(&opts.selection_file, &component, &root)?;

    let (shutdown_tx, server_task) = spawn_server(listener, ctx.clone())?;
    ctx.sink.push(format!(
        "[serve] control API listening on http://127.0.0.1:{cli_port}"
    ));
    if opts.open_browser {
        host::open_browser(&host_ctx.url);
        ctx.sink
            .push(format!("[serve] opened browser at {}", host_ctx.url));
    }
    let watcher_task = spawn_watcher(ctx.clone(), opts.selection_file.clone());

    // Supervisor: any shutdown (ServeHandle::shutdown or POST /api/shutdown)
    // flips the shared trigger; this task then kills the `ng serve` child and
    // stops the watcher, so no orphan survives the control API (C13).
    let vite_port = host_ctx.vite_port;
    let vite_url = host_ctx.url.clone();
    let host_opt = Some(host_ctx);
    let shutdown_rx = ctx.shutdown_tx.subscribe();
    let supervisor_task = tokio::spawn(async move {
        let mut rx = shutdown_rx;
        let _ = rx.changed().await;
        if let Some(mut host) = host_opt {
            let _ = host.child.kill().await;
        }
        watcher_task.abort();
    });

    Ok(ServeHandle {
        cli_port,
        vite_port,
        vite_url,
        shutdown_tx,
        server_task,
        supervisor_task,
    })
}

impl ServeHandle {
    /// Stop API server + watcher, kill the dev-server child, free both ports
    /// (C13). The supervisor owns the child: flipping the shared trigger makes
    /// it kill `ng serve` and stop the watcher.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.server_task.await;
        let _ = self.supervisor_task.await;
    }
}

/// Headless entrypoint: serve until Ctrl-C. The sink mirrors all logs to stderr.
pub async fn run_headless(opts: ServeOptions) -> anyhow::Result<()> {
    let sink = opts.sink.clone();
    let handle = start(opts).await?;
    eprintln!(
        "[render-component] UI at {} (control API http://127.0.0.1:{})",
        handle.vite_url, handle.cli_port
    );
    tokio::signal::ctrl_c().await?;
    sink.push("[serve] shutting down (Ctrl-C)");
    handle.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Strategy;
    use std::fs;
    use std::io::{Read, Write};

    fn ctx_with_fixture() -> (
        tempfile::TempDir,
        Arc<Ctx>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        // Fixture root with two valid components (like the E2E fixture).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(root.join("gallery")).unwrap();
        fs::create_dir_all(root.join("badges")).unwrap();
        let gallery = root.join("gallery/gallery.component.ts");
        fs::write(
            &gallery,
            "@Component({ selector: 'app-gallery', standalone: true, templateUrl: './gallery.component.html', styleUrls: ['./gallery.component.scss'] })\nexport class GalleryComponent {}",
        )
        .unwrap();
        fs::write(
            root.join("gallery/gallery.component.html"),
            "<p>gallery</p>",
        )
        .unwrap();
        fs::write(
            root.join("gallery/gallery.component.scss"),
            "p { color: red; }",
        )
        .unwrap();
        let badge = root.join("badges/badge.component.ts");
        fs::write(
            &badge,
            "@Component({ selector: 'app-badge', templateUrl: './badge.component.html' })\nexport class BadgeComponent {}",
        )
        .unwrap();
        fs::write(root.join("badges/badge.component.html"), "<p>badge</p>").unwrap();
        fs::write(
            root.join("badges/badge.component.module.ts"),
            "export class BadgeModule {}",
        )
        .unwrap();

        let host_tmp = tempfile::tempdir().unwrap();
        let ctx = std::sync::Arc::new(Ctx {
            app: AppState::new(
                &gallery,
                &root,
                Strategy::Standalone,
                vec![],
                serde_json::Map::new(),
            ),
            vite_port: 1,
            host_dir: host_tmp.path().to_path_buf(),
            sink: crate::logs::LogSink::new(false),
            build_pending: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ng_exited: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown_tx: tokio::sync::watch::channel(false).0,
        });
        // Leak-free ownership: keep both tempdirs alive via the returned guard.
        std::mem::forget(host_tmp);
        (tmp, ctx, gallery, badge)
    }

    fn http_get(port: u16, path: &str) -> String {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
            .unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        buf
    }

    fn http_post(port: u16, path: &str, body: &str) -> String {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req = format!(
            "POST {path} HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        buf
    }

    #[test]
    fn free_ports_are_distinct_while_bound() {
        let a = bind_free_port().unwrap();
        let b = bind_free_port().unwrap();
        let pa = a.local_addr().unwrap().port();
        let pb = b.local_addr().unwrap().port();
        assert_ne!(pa, pb, "two bound listeners cannot share a port (C04)");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn server_starts_reports_and_frees_port_on_shutdown() {
        let (_guard, ctx, _gallery, _badge) = ctx_with_fixture();
        let listener = bind_free_port().unwrap();
        let port = listener.local_addr().unwrap().port();

        let (shutdown_tx, task) = spawn_server(listener, ctx).unwrap();
        // Server is answering.
        let body = http_get(port, "/api/current");
        assert!(body.contains("gallery.component.ts"), "got: {body}");

        // Graceful shutdown (C13): after completion the port is rebindable.
        let _ = shutdown_tx.send(true);
        let _ = task.await;
        let rebind = TcpListener::bind(("127.0.0.1", port));
        assert!(rebind.is_ok(), "port {port} must be free after shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_via_api_hot_swaps_and_rejects_invalid() {
        let (_guard, ctx, _gallery, badge) = ctx_with_fixture();
        let listener = bind_free_port().unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, task) = spawn_server(listener, ctx.clone()).unwrap();

        // Hot-swap through the API (C07 transport): same server, new component.
        let body = http_post(
            port,
            "/api/select",
            &format!("{{\"component\": \"{}\"}}", badge.display()),
        );
        assert!(body.contains("\"updated\""), "got: {body}");
        let current = http_get(port, "/api/current");
        assert!(current.contains("badge.component.ts"), "got: {current}");
        assert!(current.contains("\"revision\":2"), "got: {current}");

        // Invalid component: rejected, state untouched (C10).
        let body = http_post(
            port,
            "/api/select",
            &format!(
                "{{\"component\": \"{}\"}}",
                badge.parent().unwrap().join("ghost.component.ts").display()
            ),
        );
        assert!(body.contains("\"rejected\""), "got: {body}");
        let current = http_get(port, "/api/current");
        assert!(
            current.contains("badge.component.ts"),
            "state untouched: {current}"
        );

        let _ = shutdown_tx.send(true);
        let _ = task.await;
    }
}
