mod bridge;
mod config;
mod dispatch;
mod editor;
mod epic;
mod http;
mod locks;
mod mcp;
mod project_files;
mod stdio;
mod surface;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::{PortResolver, Project};
use crate::dispatch::{Hub, HubOptions, Session};
use crate::surface::{Surface, View};

/// MCP server for the UE-MCP Unreal Engine editor bridge, built for many agents at once.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// The .uproject file, or the folder holding it. Defaults to the current directory.
    project: Option<PathBuf>,

    /// Serve Streamable HTTP on this address (e.g. 127.0.0.1:7878) instead of stdio.
    #[arg(long)]
    http: Option<SocketAddr>,

    /// Editor bridge port. Default: the port the editor published, then the per-project derived port.
    #[arg(long, env = "UE_MCP_PORT")]
    bridge_port: Option<u16>,

    /// Sockets to the editor. Each one carries a request at a time on a stock plugin.
    #[arg(long, default_value_t = 4)]
    connections: usize,

    /// Only expose these categories (comma-separated); `project` is always included.
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,

    /// Short tool descriptions (action names only). Cuts the tool list to a fraction of its size.
    #[arg(long)]
    compact: bool,

    /// Hide the epic_* wrappers of Unreal 5.8's own editor toolsets.
    #[arg(long)]
    no_epic: bool,

    /// Advertise actions even when the running plugin does not register them.
    #[arg(long)]
    show_unregistered: bool,

    /// Let any agent mutate an asset another agent locked.
    #[arg(long)]
    no_lock_enforcement: bool,

    /// Lock owner name for the stdio session.
    #[arg(long, env = "UE_MCP_AGENT")]
    agent: Option<String>,

    /// Seconds a call waits for the editor to come up before failing.
    #[arg(long, default_value_t = 15)]
    connect_wait: u64,

    /// Engine folder (the one holding Engine/) for start_editor and builds.
    /// Default: found from the .uproject's EngineAssociation.
    #[arg(long, env = "UE_ENGINE_ROOT")]
    engine_root: Option<PathBuf>,

    /// Extra argument for editors started by start_editor; repeat for several.
    /// e.g. --editor-arg=-ini:EditorSettings:[/Script/UnrealEd.EditorPerformanceSettings]:bThrottleCPUWhenNotForeground=False
    #[arg(long = "editor-arg", allow_hyphen_values = true)]
    editor_args: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // stdout is the protocol channel in stdio mode; logs always go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("UE_MCP_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let project = match &cli.project {
        Some(p) => Some(Project::resolve(p)?),
        None => Project::resolve(&std::env::current_dir()?).ok(),
    };
    match &project {
        Some(p) => tracing::info!("project: {}", p.root.display()),
        None if cli.bridge_port.is_none() => tracing::warn!("no .uproject given or found; only the default bridge port will be tried"),
        None => {}
    }

    let resolver = PortResolver { explicit: cli.bridge_port, project: project.clone() };
    let bridge = bridge::Bridge::start(resolver, cli.connections, Duration::from_secs(cli.connect_wait));
    let surface = Surface::load();
    tracing::info!("surface: {} categories, {} actions", surface.tools.len(), surface.action_count());

    let opts = HubOptions {
        enforce_locks: !cli.no_lock_enforcement,
        hide_epic: cli.no_epic,
        hide_unregistered: !cli.show_unregistered,
        engine_root: cli.engine_root,
        editor_args: cli.editor_args,
    };
    let hub = Arc::new(Hub::new(bridge, surface, opts, project));
    let view = View {
        tools: cli.tools.map(|t| t.into_iter().map(|s| s.trim().to_string()).collect::<HashSet<_>>()),
        hide_epic: cli.no_epic,
        compact: cli.compact,
        registered: None,
    };

    match cli.http {
        Some(addr) => http::serve(hub, addr, view).await,
        None => {
            let id = format!("stdio-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            let owner = cli.agent.unwrap_or_else(|| id.clone());
            stdio::serve(hub, Arc::new(Session::new(id, owner, view))).await
        }
    }
}
