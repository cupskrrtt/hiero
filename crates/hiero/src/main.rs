mod config;
mod keys;
mod notify;
mod paths;
mod widgets;
mod workspace;
mod ws_app;
mod ws_tui;
mod ws_ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hc_client::{AgentState, Client, Filter, TerminalSet, WatchMsg, Watcher, endpoint};

#[derive(Parser)]
#[command(
    name = "hiero",
    version,
    about = "A terminal workspace on top of herdr-core",
    long_about = "A terminal workspace on top of herdr-core.\n\n\
        Run `hiero` with no command to open the workspace: a sidebar of projects and named \
        workspaces, and tabs and splits of terminals (nvim, Claude Code, shells) that keep \
        running when you quit. Hiero's keys sit behind a prefix key (default ctrl+b); press \
        the prefix then ? for help."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Open the workspace even from inside one of Hiero's own panes.
    #[arg(long)]
    nested: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Low-level checks against the running core.
    #[command(subcommand)]
    Debug(Debug),
    /// Workspaces: projects, named workspaces, tabs and panes.
    #[command(subcommand)]
    Ws(WsCmd),
    /// Hiero's settings file.
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
}

#[derive(Subcommand)]
enum WsCmd {
    /// Print the tree: projects, workspaces, tabs and panes.
    List,
    /// Add a project.
    AddProject {
        name: String,
        /// Project folder (default: the current folder).
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// Add a workspace to a project.
    Add {
        project: String,
        name: String,
        /// Workspace folder (default: the project's folder).
        #[arg(long)]
        folder: Option<std::path::PathBuf>,
    },
    /// Rename a project (`<project> <new-name>`) or a workspace (`<project> <workspace> <new-name>`).
    Rename {
        #[arg(num_args = 2..=3, required = true)]
        args: Vec<String>,
    },
    /// Remove a project (`<project>`) or a workspace (`<project> <workspace>`).
    Rm {
        #[arg(num_args = 1..=2, required = true)]
        args: Vec<String>,
    },
    /// Move a project (`<project> up|down`) or a workspace (`<project> <workspace> up|down`).
    Move {
        #[arg(num_args = 2..=3, required = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print where the file is.
    Path,
    /// Write a commented starting file (keeps an existing one).
    Init,
    /// Check the file and print the settings in effect.
    Show,
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
    /// Compare Hiero's terminal emulator with herdr-core's own screen for a
    /// terminal, and list escape sequences Hiero's emulator doesn't handle.
    Vtcompare {
        terminal: String,
        /// How long to watch, in seconds.
        #[arg(long, default_value_t = 20)]
        secs: u64,
    },
    /// Show the key events your terminal sends (to check a prefix key).
    Keys,
    /// Type raw bytes into a terminal over an attach stream.
    /// Escapes: \r \n \t \e \\ and \xNN.
    Type { terminal: String, text: String },
    /// Read or write a document in the core (the workspaces are `hiero-ws`).
    Doc {
        /// JSON to store; omit to read.
        value: Option<String>,
        /// Which document; no default, so a write never lands on the wrong one.
        #[arg(long)]
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.command.is_none());
    let config_error = match config::load() {
        Ok(cfg) => {
            config::set(cfg);
            None
        }
        Err(e) => Some(e),
    };
    match cli.command {
        None => {
            if !cli.nested
                && let Some(place) = inside_hiero().await
            {
                eprintln!(
                    "You're already inside Hiero ({place}). Use the prefix key then w to switch \
                     workspaces, or run `hiero --nested` to open it here anyway."
                );
                std::process::exit(2);
            }
            ws_tui::run(config_error).await
        }
        Some(Command::Config { action }) => config_cmd(action, config_error),
        Some(Command::Debug(d)) => debug(d).await,
        Some(Command::Ws(w)) => ws_cmd(w).await,
    }
}

/// The workspace screen logs to a file (stderr would draw over the screen); commands
/// log to stderr.
fn init_logging(screen: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("HIERO_LOG").unwrap_or_else(|_| "warn".into());
    if screen {
        let dir = config::state_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hiero.log");
        // Start over past 5 MB.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
            let _ = std::fs::rename(&path, dir.join("hiero.log.old"));
        }
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

fn config_cmd(action: Option<ConfigCmd>, error: Option<String>) -> Result<()> {
    let path = config::config_path();
    match action.unwrap_or(ConfigCmd::Show) {
        ConfigCmd::Path => println!("{}", path.display()),
        ConfigCmd::Init => {
            if path.exists() {
                println!("{} already exists; left as is", path.display());
            } else {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, config::DEFAULT_TOML)?;
                println!("wrote {}", path.display());
            }
        }
        ConfigCmd::Show => {
            if let Some(e) = error {
                anyhow::bail!("{e}");
            }
            let exists = if path.exists() { "" } else { " (not created yet; defaults in effect)" };
            println!("# {}{exists}", path.display());
            if let Some(w) = config::get().ignored_warning() {
                println!("# warning: {w}");
            }
            print!("{}", toml::to_string_pretty(config::get())?);
            println!("# logs: {}", config::state_dir().join("hiero.log").display());
        }
    }
    Ok(())
}

/// An existing folder, as an absolute path.
fn existing_dir(p: &std::path::Path) -> Result<String> {
    let abs = paths::canonical(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
    if !std::path::Path::new(&abs).is_dir() {
        anyhow::bail!("{abs} is not a folder");
    }
    Ok(abs)
}

async fn ws_cmd(cmd: WsCmd) -> Result<()> {
    use workspace::{Move, WsOp, new_id};
    let hc = Client::connect().await?;
    let doc = workspace::load(&hc).await.map_err(anyhow::Error::msg)?;
    let project = |name: &str| {
        doc.project_by_name(name).ok_or_else(|| anyhow::anyhow!("no project named \"{name}\""))
    };
    let op = match cmd {
        WsCmd::List => {
            print!("{}", workspace::render_tree(&doc));
            return Ok(());
        }
        WsCmd::AddProject { name, root } => {
            let root = match root {
                Some(r) => r,
                None => std::env::current_dir()?,
            };
            WsOp::AddProject { id: new_id("p"), name, root: existing_dir(&root)? }
        }
        WsCmd::Add { project: pname, name, folder } => {
            let p = project(&pname)?;
            let folder = match folder {
                Some(f) => existing_dir(&f)?,
                None => existing_dir(std::path::Path::new(&p.root))?,
            };
            WsOp::AddWorkspace {
                project: p.id.clone(),
                id: new_id("w"),
                name,
                folder,
                tab_id: new_id("t"),
                pane_id: new_id("n"),
            }
        }
        WsCmd::Rename { args } => {
            let p = project(&args[0])?;
            match &args[..] {
                [_, new] => WsOp::RenameProject { id: p.id.clone(), name: new.clone() },
                [_, w, new] => WsOp::RenameWorkspace { id: find_ws(p, w)?, name: new.clone() },
                _ => unreachable!("clap allows 2 or 3 arguments"),
            }
        }
        WsCmd::Rm { args } => {
            let p = project(&args[0])?;
            match &args[..] {
                [_] => WsOp::DeleteProject { id: p.id.clone() },
                [_, w] => WsOp::DeleteWorkspace { id: find_ws(p, w)? },
                _ => unreachable!("clap allows 1 or 2 arguments"),
            }
        }
        WsCmd::Move { args } => {
            let p = project(&args[0])?;
            let dir = match args.last().map(String::as_str) {
                Some("up") => Move::Up,
                Some("down") => Move::Down,
                _ => anyhow::bail!("the last argument must be up or down"),
            };
            match &args[..] {
                [_, _] => WsOp::MoveProject { id: p.id.clone(), dir },
                [_, w, _] => WsOp::MoveWorkspace { id: find_ws(p, w)?, dir },
                _ => unreachable!("clap allows 2 or 3 arguments"),
            }
        }
    };
    workspace::update(&hc, &[op]).await.map_err(anyhow::Error::msg)?;
    println!("ok");
    Ok(())
}

fn find_ws(p: &workspace::Project, name: &str) -> Result<String> {
    p.workspace_by_name(name)
        .map(|w| w.id.clone())
        .ok_or_else(|| anyhow::anyhow!("project \"{}\" has no workspace named \"{name}\"", p.name))
}

/// Where this process runs if it is inside one of Hiero's own panes:
/// "project › workspace", or a generic name when the doc can't say.
async fn inside_hiero() -> Option<String> {
    let hc = tokio::time::timeout(std::time::Duration::from_secs(2), Client::connect()).await.ok().and_then(Result::ok);
    let mut pane = std::env::var(ws_app::ENV_PANE).ok().filter(|s| !s.is_empty());
    if pane.is_none() {
        // Panes started before HIERO_PANE existed carry only the label.
        let me = std::env::var("HC_TERMINAL_ID").ok().filter(|s| !s.is_empty())?;
        pane = hc.as_ref()?.get(&me).await.ok()?.labels.get(ws_app::LABEL_PANE).cloned();
    }
    let pane = pane?;
    let name = async {
        let doc = workspace::load(hc.as_ref()?).await.ok()?;
        doc.projects.iter().find_map(|p| {
            p.workspaces
                .iter()
                .find(|w| w.panes().iter().any(|x| x.id == pane))
                .map(|w| format!("{} › {}", p.name, w.name))
        })
    }
    .await;
    Some(name.unwrap_or_else(|| "a Hiero pane".into()))
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
        Debug::Keys => debug_keys()?,
        Debug::Vtcompare { terminal, secs } => vtcompare(&terminal, secs).await?,
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

/// Print each key event as crossterm sees it, and whether it is the prefix.
/// Records what Hiero's emulator (vt100) could not interpret.
#[derive(Default)]
struct Unhandled(std::collections::BTreeMap<String, usize>);

impl vt100::Callbacks for Unhandled {
    fn unhandled_char(&mut self, _: &mut vt100::Screen, c: char) {
        *self.0.entry(format!("char {c:?}")).or_default() += 1;
    }
    fn unhandled_control(&mut self, _: &mut vt100::Screen, b: u8) {
        *self.0.entry(format!("control 0x{b:02x}")).or_default() += 1;
    }
    fn unhandled_escape(&mut self, _: &mut vt100::Screen, i1: Option<u8>, i2: Option<u8>, b: u8) {
        let i: String = [i1, i2].into_iter().flatten().map(char::from).collect();
        *self.0.entry(format!("ESC {i}{}", b as char)).or_default() += 1;
    }
    fn unhandled_csi(&mut self, _: &mut vt100::Screen, i1: Option<u8>, i2: Option<u8>, params: &[&[u16]], c: char) {
        let i: String = [i1, i2].into_iter().flatten().map(char::from).collect();
        let p: Vec<String> = params.iter().map(|g| g.iter().map(u16::to_string).collect::<Vec<_>>().join(":")).collect();
        *self.0.entry(format!("CSI {i}{}{c}", p.join(";"))).or_default() += 1;
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let head = params.first().map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_default();
        *self.0.entry(format!("OSC {head}")).or_default() += 1;
    }
}

/// Watch a terminal as an observer, feed its output to vt100, and every
/// second compare the screen with herdr-core's (Ghostty's) rendering.
async fn vtcompare(terminal: &str, secs: u64) -> Result<()> {
    use hc_client::{AttachParams, AttachStream, StreamMsg};
    let hc = Client::connect().await?;
    let t = hc.get(terminal).await?;
    let (cols, rows) = (t.cols.max(1), t.rows.max(1));
    let mut params = AttachParams::new(terminal, cols, rows);
    params.observe = true;
    let mut stream = AttachStream::open(&params).await?;
    let nz = |n: u16| std::num::NonZeroU16::new(n).unwrap();
    let mut parser = vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 0, Unhandled::default());
    println!("watching {terminal} ({cols}x{rows}) for {secs}s; use the program now");
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut worst = 0usize;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(end) => break,
            msg = stream.recv() => match msg {
                StreamMsg::Data(bytes) => parser.process(&bytes),
                StreamMsg::Closed(_) => break,
                StreamMsg::Resize { .. } => {}
            },
            _ = tick.tick() => {
                let theirs = hc.read(terminal, 0).await?.text;
                let theirs: Vec<&str> = theirs.lines().collect();
                let ours: Vec<String> = parser.screen().rows(0, cols).collect();
                let differ: Vec<usize> = (0..rows as usize)
                    .filter(|&i| ours.get(i).map(|s| s.trim_end()).unwrap_or("") != theirs.get(i).map(|s| s.trim_end()).unwrap_or(""))
                    .collect();
                if differ.len() > worst {
                    worst = differ.len();
                    println!("\n{} differing row(s):", differ.len());
                    for &i in differ.iter().take(6) {
                        println!("  row {i:>3} hiero: {:?}", ours.get(i).map(|s| s.trim_end()).unwrap_or(""));
                        println!("          herdr: {:?}", theirs.get(i).map(|s| s.trim_end()).unwrap_or(""));
                    }
                }
            }
        }
    }
    stream.close().await;
    println!("\nmost differing rows seen: {worst}");
    let unhandled = &parser.callbacks().0;
    if unhandled.is_empty() {
        println!("vt100 handled every sequence");
    } else {
        println!("sequences vt100 did not handle (count):");
        for (k, n) in unhandled {
            println!("  {n:>6}  {k}");
        }
    }
    Ok(())
}

fn debug_keys() -> Result<()> {
    use crossterm::event::{Event, KeyCode, read};
    let prefix = config::get().prefix();
    println!("Press keys to see what Hiero receives (prefix is {}). Esc twice quits.\r", prefix.label());
    crossterm::terminal::enable_raw_mode()?;
    let result = (|| -> Result<()> {
        let mut last_esc = false;
        loop {
            if let Event::Key(k) = read()? {
                if !keys::is_input(k.kind) {
                    continue;
                }
                let tag = if prefix.matches(&k) { "  <- PREFIX" } else { "" };
                println!("{:?} {:?}{tag}\r", k.code, k.modifiers);
                if k.code == KeyCode::Esc {
                    if last_esc {
                        return Ok(());
                    }
                    last_esc = true;
                } else {
                    last_esc = false;
                }
            }
        }
    })();
    crossterm::terminal::disable_raw_mode()?;
    result
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
