use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

/// Render a single UI component in the browser from a navigable file explorer.
#[derive(Parser, Debug)]
#[command(name = "render-component", version)]
pub struct Cli {
    /// Project root directory to explore (defaults to the current directory).
    pub path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Serve a preselected component without the TUI (scriptable / E2E entrypoint).
    Serve {
        /// Component source file to render (e.g. src/app/foo.component.ts).
        #[arg(long)]
        component: PathBuf,

        /// Project root the component lives in (defaults to the component's parent dir).
        #[arg(long)]
        root: Option<PathBuf>,

        /// Fixed port for the control server (defaults to a free port).
        #[arg(long)]
        port: Option<u16>,

        /// Where the selection IPC file lives (defaults to a temp file).
        #[arg(long)]
        selection_file: Option<PathBuf>,

        /// Do not open the browser automatically.
        #[arg(long, default_value_t = false)]
        no_open: bool,
    },
}

/// Validate that a candidate project root is a usable directory (C14: early feedback).
pub fn validate_root(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot open project root '{}': {e}", path.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("'{}' is not a directory", canonical.display());
    }
    Ok(canonical)
}

/// Validate that a candidate component file exists (C14: early feedback).
pub fn validate_component(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot open component '{}': {e}", path.display()))?;
    if !canonical.is_file() {
        anyhow::bail!("'{}' is not a file", canonical.display());
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_root() {
        let err = validate_root(Path::new("/definitely/not/a/real/dir/xyz"));
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("cannot open project root"), "got: {msg}");
    }

    #[test]
    fn rejects_file_as_root() {
        let err = validate_root(Path::new("Cargo.toml"));
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("not a directory"), "got: {msg}");
    }

    #[test]
    fn accepts_real_directory() {
        let dir = validate_root(Path::new(".")).expect("cwd is a directory");
        assert!(dir.is_dir());
    }

    #[test]
    fn rejects_missing_component() {
        let err = validate_component(Path::new("/definitely/not/a/component.ts"));
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("cannot open component"), "got: {msg}");
    }
}
