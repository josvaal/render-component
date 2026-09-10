mod cli;
mod detect;
mod explorer;
mod host;
mod logs;
mod pipeline;
mod pipeline_types;
mod serve;

use std::path::PathBuf;

use clap::Parser;

use cli::{Cli, Command};

fn default_selection_file() -> PathBuf {
    std::env::temp_dir().join("render-component-selection.json")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve {
            component,
            root,
            port,
            selection_file,
            no_open,
        }) => {
            let root = root.unwrap_or_else(|| {
                component
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            });
            let selection_file = selection_file.unwrap_or_else(default_selection_file);
            serve::run_headless(serve::ServeOptions {
                component,
                root,
                port,
                selection_file,
                open_browser: !no_open,
                sink: serve::LogSink::new(true),
            })
            .await
        }
        None => explorer::run(cli).await,
    }
}
