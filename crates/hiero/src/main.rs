mod app;
mod attach;
mod forms;
mod git;
mod integrate;
mod keys;
mod notify;
mod pipeline;
mod tasks;
mod tui;
mod ui;
mod worktree;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hc_client::{AgentState, Client, Filter, TerminalSet, WatchMsg, Watcher, endpoint};

#[derive(Parser)]
#[command(name = "hiero", version, about = "A TUI agent manager on top of herdr-core")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Low-level checks against the running core.
    #[command(subcommand)]
    Debug(Debug),
    /// Install or remove the status bridge for an agent.
    Integrate {
        /// The agent: claude.
        agent: String,
        /// Remove Hiero's hooks instead.
        #[arg(long)]
        uninstall: bool,
        /// Only show what's installed.
        #[arg(long)]
        status: bool,
        /// Settings file (default: ~/.claude/settings.json).
        #[arg(long)]
        settings: Option<std::path::PathBuf>,
        /// Hiero binary the hooks run (default: this one).
        #[arg(long)]
        exe: Option<std::path::PathBuf>,
        /// Install even if the agent reports status by itself.
        #[arg(long)]
        force: bool,
    },
    /// Report a status for the hc terminal this runs in (for any agent or script).
    Report {
        /// idle, working, done, blocked, error or clear.
        state: String,
        /// For blocked: permission, question or auth.
        #[arg(long)]
        kind: Option<String>,
        /// A short message shown on the board.
        #[arg(long)]
        msg: Option<String>,
        /// Who is reporting.
        #[arg(long, default_value = "hiero")]
        app: String,
    },
    /// Send one message to many live agents.
    Broadcast {
        /// The message.
        text: String,
        /// Only terminals with this label (key=value; repeatable).
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        /// Type even into agents that are blocked.
        #[arg(long)]
        force: bool,
    },
    /// Called by agent hooks; reports status to herdr-core.
    #[command(hide = true)]
    Hook { agent: String, event: String, arg: Option<String> },
}

#[derive(Subcommand)]
enum Debug {
    /// Handshake and server status.
    Status,
    /// List terminals.
    Ls {
        /// Include finished terminals.
        #[arg(long)]
        all: bool,
    },
    /// Print the screen of a terminal.
    Read {
        terminal: String,
        #[arg(long, default_value_t = 0)]
        lines: usize,
    },
    /// Follow live events (reconnects if the core restarts).
    Events {
        /// Print raw event data too.
        #[arg(long)]
        raw: bool,
    },
    /// Type raw bytes into a terminal over an attach stream.
    /// Escapes: \r \n \t \e \\ and \xNN.
    Type { terminal: String, text: String },
    /// Read or write Hiero's document in the core.
    Doc {
        /// JSON to store; omit to read.
        value: Option<String>,
        #[arg(long, default_value = "hiero")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HIERO_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        None => tui::run().await,
        Some(Command::Debug(d)) => debug(d).await,
        Some(Command::Hook { agent, event, arg }) => {
            integrate::run_hook(&agent, &event, arg.as_deref()).await;
            Ok(())
        }
        Some(Command::Broadcast { text, labels, force }) => broadcast(&text, &labels, force).await,
        Some(Command::Report { state, kind, msg, app }) => {
            integrate::report_cmd(&state, kind.as_deref(), msg.as_deref(), &app).await
        }
        Some(Command::Integrate { agent, uninstall, status, settings, exe, force }) => {
            integrate_cmd(&agent, uninstall, status, settings, exe, force)
        }
    }
}

async fn broadcast(text: &str, labels: &[String], force: bool) -> Result<()> {
    let mut filter = hc_client::Labels::new();
    for l in labels {
        let (k, v) = l.split_once('=').ok_or_else(|| anyhow::anyhow!("label must be KEY=VALUE: {l}"))?;
        filter.insert(k.to_string(), v.to_string());
    }
    let hc = Client::connect().await?;
    let me = std::env::var("HC_TERMINAL_ID").ok();
    let targets: Vec<_> = hc
        .list(false, &filter)
        .await?
        .terminals
        .into_iter()
        .filter(|t| Some(&t.address) != me.as_ref())
        .collect();
    if targets.is_empty() {
        println!("no live terminals match");
        return Ok(());
    }
    let mut failed = 0;
    for t in &targets {
        match hc.send_with(&t.address, text, force).await {
            Ok(_) => println!("✓ {} {}", t.address, t.display_name()),
            Err(e) => {
                failed += 1;
                println!("✗ {} {}: {e}", t.address, t.display_name());
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} of {} failed", targets.len());
    }
    Ok(())
}

fn integrate_cmd(
    agent: &str,
    uninstall: bool,
    status: bool,
    settings: Option<std::path::PathBuf>,
    exe: Option<std::path::PathBuf>,
    force: bool,
) -> Result<()> {
    let exe = match exe {
        Some(e) => e,
        None => std::env::current_exe()?.canonicalize()?,
    };
    if agent == "codex" {
        let q = |s: &str| serde_json::to_string(s).unwrap();
        println!("Codex reports only finished turns, through `notify`. Add this to ~/.codex/config.toml:\n");
        println!("notify = [{}, \"hook\", \"codex\"]\n", q(&exe.to_string_lossy()));
        println!("Codex sessions in hc terminals will then show `done` when a turn ends.");
        return Ok(());
    }
    if agent != "claude" {
        anyhow::bail!("supported: claude, codex. Other agents can call `hiero report <state>` from their own hooks.");
    }
    let path = settings.unwrap_or_else(integrate::claude_settings_path);
    if status {
        let hooks = integrate::claude_status(&path)?;
        if hooks.is_empty() {
            println!("not installed in {}", path.display());
        } else {
            println!("installed in {}:", path.display());
            for (event, cmd) in hooks {
                println!("  {event:<18} {cmd}");
            }
        }
        return Ok(());
    }
    if uninstall {
        let n = integrate::uninstall_claude(&path)?;
        println!("removed {n} hiero hook(s) from {}", path.display());
        return Ok(());
    }
    if !force && let Some(v) = integrate::claude_version()
        && v >= integrate::CLAUDE_NATIVE_SINCE
    {
        println!(
            "Claude Code {}.{}.{} reports working / blocked / done by itself, so no hooks are needed.",
            v.0, v.1, v.2
        );
        println!("Hiero's board already shows it. Use --force to install the hooks anyway (they stay quiet");
        println!("while Claude reports natively).");
        return Ok(());
    }
    integrate::install_claude(&path, &exe)?;
    println!("installed hiero status hooks in {}", path.display());
    println!("  events: {}", integrate::CLAUDE_EVENTS.join(", "));
    println!("  runs:   {} hook claude <event>", exe.display());
    println!("Other hooks were kept; the previous file is saved as settings.json.hiero-backup.");
    println!("New Claude sessions in hc terminals will report working / blocked / done.");
    if exe.components().any(|c| c.as_os_str() == "target") {
        println!("Note: this points at a build folder; reinstall if you move or clean it.");
    }
    Ok(())
}

async fn debug(cmd: Debug) -> Result<()> {
    match cmd {
        Debug::Status => {
            let hc = Client::connect().await?;
            let hello = hc.hello().await?;
            let st = hc.status().await?;
            println!("socket     {}", endpoint::socket_path().display());
            println!("core       {} ({})", hello.core.name, hello.core.id);
            println!("version    {} (protocol gen {})", hello.version, hello.generation);
            println!("pid        {}  uptime {}s", st.pid, st.uptime_ms / 1000);
            println!("terminals  {} live, {} finished", st.live, st.finished);
            println!("features   {}", hello.features.join(", "));
        }
        Debug::Ls { all } => {
            let hc = Client::connect().await?;
            let list = hc.list(all, &Default::default()).await?;
            println!("{:<8} {:<16} {:<9} {:<8} CWD / CMD", "ADDR", "NAME", "STATE", "STATUS");
            for t in &list.terminals {
                println!(
                    "{:<8} {:<16} {:<9} {:<8} {}  {}",
                    t.address,
                    t.name.as_deref().unwrap_or("-"),
                    format!("{:?}", t.state).to_lowercase(),
                    status_word(t),
                    t.cwd,
                    t.argv.join(" ")
                );
            }
            if list.hidden_finished > 0 {
                println!("({} finished hidden; --all shows them)", list.hidden_finished);
            }
        }
        Debug::Read { terminal, lines } => {
            let hc = Client::connect().await?;
            print!("{}", hc.read(&terminal, lines).await?.text);
        }
        Debug::Events { raw } => {
            let mut rx = Watcher::spawn(endpoint::socket_path(), Filter::default());
            let mut set = TerminalSet::new();
            eprintln!("following events from {} (ctrl+c to stop)", endpoint::socket_path().display());
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    msg = rx.recv() => {
                        let Some(msg) = msg else { break };
                        set.apply_watch(&msg);
                        match &msg {
                            WatchMsg::Snapshot { seq, terminals } => {
                                println!("[snapshot] seq={seq} terminals={}", terminals.len());
                            }
                            WatchMsg::Disconnected { error } => println!("[disconnected] {error}; retrying"),
                            WatchMsg::Event(e) => {
                                let who = e.terminal.as_deref().unwrap_or("-");
                                let name = e.terminal.as_deref()
                                    .and_then(|a| set.get(a))
                                    .map(|t| t.display_name().to_string())
                                    .unwrap_or_default();
                                if raw {
                                    println!("[{:>5}] {who:<6} {name:<14} {:?}", e.seq, e.data);
                                } else {
                                    println!("[{:>5}] {who:<6} {name:<14} {}", e.seq, summarize(&e.data));
                                }
                            }
                        }
                    }
                }
            }
        }
        Debug::Type { terminal, text } => {
            let hc = Client::connect().await?;
            let t = hc.get(&terminal).await?;
            let params = hc_client::AttachParams::new(&terminal, t.cols, t.rows);
            let mut stream = hc_client::AttachStream::open(&params).await?;
            // Drain the first paint so the core sees an active viewer.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(200), stream.recv()).await;
            stream.input(&unescape(&text)).await?;
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            stream.close().await;
        }
        Debug::Doc { value, name } => {
            let hc = Client::connect().await?;
            let cur = hc.doc_get(&name).await?;
            match value {
                None => println!("v{} {}", cur.version, serde_json::to_string_pretty(&cur.value)?),
                Some(v) => {
                    let v: serde_json::Value = serde_json::from_str(&v)?;
                    let set = hc.doc_set(&name, &v, cur.version).await?;
                    println!("wrote v{}", set.version);
                }
            }
        }
    }
    Ok(())
}

fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'r' => out.push(b'\r'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'e' => out.push(0x1b),
                b'x' if i + 2 < b.len() => {
                    if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                        out.push(v);
                    }
                    i += 2;
                }
                c => out.push(c),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    out
}

fn status_word(t: &hc_client::Terminal) -> String {
    match t.agent_state() {
        AgentState::Unknown => "-".into(),
        s => format!("{s:?}").to_lowercase(),
    }
}

fn summarize(d: &hc_client::EventData) -> String {
    use hc_client::EventData::*;
    match d {
        Created(t) => format!("created  {}", t.argv.join(" ")),
        Restarted(_) => "restarted (new shell)".into(),
        Lost(e) => format!("lost     {:?}", e.reason),
        Finished(e) => format!("finished {} code={:?}", e.reason.as_deref().unwrap_or("?"), e.exit_code),
        Removed => "removed".into(),
        Keep { keep } => format!("keep     {keep}"),
        Status { status, .. } => match status {
            Some(s) if !s.msg.is_empty() => format!("status   {} — {}", s.state, s.msg),
            Some(s) => format!("status   {}", s.state),
            None => "status   cleared".into(),
        },
        Turn { turn, state } => format!("turn     #{turn} ended {state}"),
        Title { title } => format!("title    {title}"),
        Labels { labels, .. } => format!("labels   {labels:?}"),
        Size { cols, rows } => format!("size     {cols}x{rows}"),
        Foreground(Some(f)) => format!("fg       {} in {}", f.name, f.cwd),
        Foreground(None) => "fg       -".into(),
        Doc { name, version } => format!("doc      {name} v{version}"),
        Unknown { kind, .. } => format!("{kind} (unknown)"),
    }
}
