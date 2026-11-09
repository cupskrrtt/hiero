# hiero

A terminal workspace on top of [herdr-core](https://github.com/herdrdev/herdr-core) (`hc`).

Hiero shows projects and named workspaces in a sidebar. Each workspace is a set of tabs and
splits, and every pane is a terminal (nvim, Claude Code, a shell) that herdr-core keeps running
in the background. Quit Hiero, come back later, and the layout and the programs are still there.

```
┌ projects ──────────┐  1 editor   2 agents   3 shell
│▾ shosha            │┌ nvim . t4 ──────────────────┐┌ claude t5 ─────────────────┐
│  ▶ ● frontend      ││                             ││ ◐ approve Write: src/a.rs  │
│    ◐ backend       ││  fn main() {                ││                            │
│    ✓ docs          ││                             │└────────────────────────────┘
│▸ solo ·            ││                             │┏ shell t6 ━━━━━━━━━━━━━━━━━━┓
│                    ││                             │┃ $ cargo test               ┃
└────────────────────┘└─────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
 ctrl+b then  t tab · v s split · hjkl move · z zoom · ? help · q quit
```

Hiero keeps no database of its own. Terminals live in herdr-core, and Hiero's data (projects,
workspaces, layouts) lives in herdr-core's document store.

Hiero's own keys sit behind a prefix key (default `ctrl+b`), so nvim and Claude get every other
key.

## Install

Needs [herdr-core](https://github.com/herdrdev/herdr-core) running (`hc server status`).

```sh
# Arch: build and install a package from this checkout
cd packaging/arch && makepkg -si

# from GitHub, with Rust
cargo install --locked --git https://github.com/cupskrrtt/hiero hiero

# from a checkout, with Rust
cargo install --path crates/hiero
```

## Windows

Hiero runs natively on Windows (not only in WSL).

Requirements:

- [herdr-core](https://github.com/herdrdev/herdr-core) for Windows (`hc.exe`), running: `hc server status`.
- Rust with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`) and the Visual Studio C++ build tools.

Install, in PowerShell:

```powershell
cargo install --locked --git https://github.com/cupskrrtt/hiero hiero
# or from a checkout
cargo install --path crates\hiero
```

Hiero finds the core on the named pipe `\\.\pipe\herdr-core-<user>-<session>.sock` (`<user>` is
`%USERNAME%` with only letters and digits; `<session>` is `HC_SESSION` or `default`). `HC_SOCKET`
overrides it. A `socket` key in herdr-core's `config.toml` is not read.

| Path | What |
|---|---|
| `%APPDATA%\hiero\config.toml` | settings |
| `%LOCALAPPDATA%\hiero\ui.json`, `hiero.log` | remembered UI state, log |

`XDG_CONFIG_HOME` and `XDG_STATE_HOME`, when set, win over these. Folders are stored as plain
`C:\...` paths; `~` means `%USERPROFILE%`.

- A pane without a command starts PowerShell 7 (or Windows PowerShell), chosen by herdr-core.
- **Windows and WSL are two separate cores.** Native Windows Hiero and Hiero inside WSL talk to
  different herdr-core servers, with separate terminals, workspaces and documents.
- The Windows core keeps running after you close the window, and ends when you sign out or
  restart. With `restore = true`, opening a workspace afterwards starts its terminals again in
  their layout.
- Desktop notifications are a PowerShell toast (plus the bell).

## Quick start

1. Run `hiero`. The sidebar is empty.
2. `A` adds a project: a name and a folder.
3. `a` adds a workspace under it. `Enter` opens it. A shell starts in the folder.
4. `ctrl+b v` splits the pane side by side, `ctrl+b s` stacks one. `ctrl+b V` and `S` ask for a
   command first: type `nvim .` or `claude`. `ctrl+b t` opens a new tab, `T` asks for a command.
5. `ctrl+b h j k l` moves between panes. `ctrl+b ?` shows every key.
6. `ctrl+b q` quits Hiero. The programs keep running.
7. Run `hiero` again. The same workspace opens and every pane attaches to its running terminal.

## Keys

After the prefix (`ctrl+b`) the bottom bar shows `prefix` until the next key.

| Key | Action |
|---|---|
| `t` / `T` | new tab with a shell / ask for a command first |
| `v` / `s` | split side by side / stacked (new shell, same folder, gets focus) |
| `V` / `S` | same, but ask for a command first |
| `h` `j` `k` `l` or arrows | move focus to the neighbouring pane |
| `H` `J` `K` `L` | narrower / taller / shorter / wider (5% a press) |
| `1` … `9` | go to tab N |
| `[` / `]` | previous / next tab |
| `r` | rename the tab |
| `R` | rename the pane (empty brings back the automatic title) |
| `x` / `X` | close the pane / the tab (asks; its programs end) |
| `z` | zoom the pane to the whole area, again to undo (a view only, not saved) |
| `w` | switch between sidebar and workspace (shows a hidden sidebar) |
| `b` | hide or show the sidebar |
| `<` / `>` | narrower / wider sidebar (4 columns a step, 16 to 60) |
| `n` / `N` | new workspace in this project / new project |
| `?` | help |
| `q` | quit Hiero (terminals keep running) |
| prefix again | send the prefix key itself to the program |
| `Esc` | cancel |

The last pane of a tab closes the tab. The last tab of a workspace can't be closed; delete the
workspace instead. Sidebar width and hidden state are remembered.

In the sidebar (no prefix needed):

| Key | Action |
|---|---|
| `j` `k` / `↑` `↓`, `g` `G` | move |
| `Enter` | open a workspace; on a project, fold or unfold it |
| `a` / `A` | add workspace under this project / add project |
| `r` | rename |
| `D` | delete (asks: `y` end its terminals · `k` keep them running · `n` cancel) |
| `J` / `K` | move down / up |
| `z` | fold or unfold the project |
| `Esc`, `l`, `Tab` | back to the workspace |
| `q` | quit |

Mouse (the workspace screen):

| Mouse | Action |
|---|---|
| click a workspace in the sidebar | open it |
| click a project | fold or unfold it |
| click a tab | go to it |
| click a pane | focus it |
| drag the line between two panes | resize the split (saved when you let go) |
| wheel over the sidebar | move the selection |
| wheel over a pane | scroll a program that takes the mouse; arrows for a full-screen one (less, man) |

If the program in a pane turned on mouse reporting (nvim with `mouse=a`, htop, tmux), clicks,
drags and the wheel go to it, with positions relative to the pane. Other panes only get focus.
Hold **Shift** while dragging to select text with the terminal's own selection (Ghostty, kitty,
WezTerm, Windows Terminal, xterm and most others). Dialogs and the help ignore the mouse. Turn it
all off with `mouse = false` under `[workspace]`.

If a prefix key does not seem to work, `hiero debug keys` shows what your terminal sends.

## Workspaces in depth

**Projects and workspaces.** A project has a root folder. A workspace has its own folder (the
project's root by default). Names are unique among siblings. Both can be renamed, reordered and
deleted from the sidebar, or with `hiero ws …`; changes from the CLI show up live.

**Panes.** Every pane is an hc terminal with labels `hiero.ws`, `hiero.pane` and `hiero.project`.
The focused pane has a thick border and the cursor. Keys and paste go to it only. A pane that was
started with a command stores it, so it can be started again.

**Layouts.** Tabs, splits and sizes are stored in herdr-core's document `hiero-ws`, so they
survive closing Hiero, a core restart and a reboot. The open workspace is remembered.

**Restore rule.** When a workspace opens, every pane of every tab gets a running terminal, not only
the visible tab. Missing ones start in the background (the pane's command or a shell, in its
folder). A pane's terminal starts again by itself when it is gone from hc, or when it finished
because the system went down (`server_stop`, `host_shutdown`, `holder_lost`,
`stopped_by_restart`). If its program exited or you killed it, the pane stays as `ended: …` and
`Enter` starts a fresh one. Nothing starts twice, and nothing starts before Hiero has seen the
terminal list.

**Core restart while Hiero is open.** The pane area shows a red banner while the core is away.
When it is back, Hiero checks every pane with the same rule: missing terminals start again, a
stream that was only lost attaches again, and programs you ended stay ended. The bar says
`core restarted — restored 3 panes`. Restored programs start from their command; what was on their
screen is gone.

**Nesting.** Every pane's environment has `HIERO_PANE` and `HIERO_WS`. Running `hiero` inside one
of its own panes stops with a message (*You're already inside Hiero (shosha › backend)…*) instead of
opening a second workspace screen; use the prefix then `w` to switch. `hiero --nested` opens it
anyway. Even then Hiero never attaches the terminal it runs in: that pane shows
`ended: this is Hiero itself` rather than a picture of itself drawing itself. Older panes without
the variables are recognised by their `hiero.pane` label. Commands like `hiero ws` work normally inside panes.

**Delete.** `D` on a workspace or project asks what to do with its terminals: `y` ends them and
deletes, `k` deletes and leaves them running, `n` or `Esc` cancels.

**Status dots.** The sidebar shows `●` working, `◐` needs you, `✓` done, `○` idle, `·` no terminal,
per workspace and on a collapsed project. The bottom bar names a workspace that needs you, also
when the sidebar is hidden. Changes to needs-you or done ring the bell and notify, following the
`[notify]` rules.

**`[workspace]` config.** `prefix` is `ctrl+<letter>` (not c, h, i, j, m) or `ctrl+space`.
`restore = false` turns the automatic start off: panes without a terminal then say "not running"
and `Enter` starts one. `mouse = false` leaves the mouse to your terminal.

## Configuration

`~/.config/hiero/config.toml`. Every setting has a default, so the file can stay short.
`hiero config init` writes a commented starting file, `hiero config show` checks it and prints what
is in effect. A broken file is reported and ignored.

Older files may still have the `[board]`, `[agents]` and `[keys]` sections from the removed agent
board. Hiero skips them with a warning (shown for a few seconds in the bottom bar, logged, and
printed by `hiero config show`); delete them when you like.

```toml
[workspace]
prefix = "ctrl+b"            # ctrl+<letter> (not c, h, i, j, m) or ctrl+space
restore = true               # start missing terminals when a workspace opens
mouse = true                 # clicks, wheel, border drags; Shift+drag still selects text

[notify]
bell = true
desktop = true               # notify-send, or a Windows toast (Windows and WSL)
on = ["blocked", "error", "done", "failed"]   # also: exited
```

## CLI reference

```sh
hiero                                  # the workspace
hiero --nested                         # the workspace, even from inside a Hiero pane

# workspaces (the same edits are in the UI)
hiero ws add-project shosha --root ~/code/shosha     # root defaults to the current folder
hiero ws add shosha frontend --folder ~/code/shosha/web   # folder defaults to the project root
hiero ws list                          # projects > workspaces > tabs > panes
hiero ws rename shosha frontend web    # or: rename shosha shosha-app
hiero ws move shosha web up            # or: move shosha down
hiero ws rm shosha web                 # or: rm shosha

hiero config [path | init | show]

hiero debug status                     # handshake and core info
hiero debug ls --all                   # terminals
hiero debug events                     # follow live events
hiero debug read t1 --lines 50         # a terminal's screen
hiero debug keys                       # what your terminal sends (to check a prefix key)
hiero debug type t1 'ls\r'             # type raw bytes over an attach stream (\r \n \t \e \xNN)
hiero debug doc --name hiero-ws [JSON]  # read or write a document (the name is required)
hiero debug vtcompare t4 [--secs 20]   # compare Hiero's emulator with herdr-core's screen for a terminal
```

Projects and workspaces are named exactly on the command line. Folders must exist.

## Environment variables

| Variable | Meaning |
|---|---|
| `HC_SOCKET` | Use this core socket (on Windows a pipe name like `\\.\pipe\herdr-core-me-x.sock`) |
| `HC_SESSION` | Use a separate hc session (default `default`) |
| `HIERO_LOG` | Log level, for example `debug` or `hc_client=trace` |
| `HIERO_NO_NOTIFY` | Turn off desktop notifications (the bell still rings) |
| `XDG_CONFIG_HOME` | Where `hiero/config.toml` lives (default `~/.config`, Windows `%APPDATA%`) |
| `XDG_STATE_HOME` | Where `hiero/hiero.log` and `hiero/ui.json` live (default `~/.local/state`, Windows `%LOCALAPPDATA%`) |
| `HIERO_PANE` / `HIERO_WS` | Set by Hiero inside its panes (pane and workspace id); used to refuse nesting |

## Files

| Path | What |
|---|---|
| `~/.config/hiero/config.toml` | settings |
| `~/.local/state/hiero/ui.json` | remembered: open workspace, sidebar width and state |
| `~/.local/state/hiero/hiero.log` | log of the UI (rotated past 5 MB) |
| herdr-core doc `hiero-ws` | projects, workspaces, tabs, splits, pane commands and terminals |

Old versions kept tasks and pipeline runs in a herdr-core doc named `hiero`; nothing reads it now.
Reset the workspaces with `hiero debug doc null --name hiero-ws`. This forgets every layout;
running terminals are not touched.

## Known issues

| # | Issue | Impact | Plan |
|---|---|---|---|
| 1 | Before its first report a program shows `·`, including while Claude waits at its "trust this folder?" dialog. Hiero reads only what a program reports to herdr-core (OSC 7501); Claude Code 2.1.319 and later does, older versions and other agents may not. | A new pane in a new folder looks idle while it waits for you. | Show "starting / not reported" for young panes. |
| 2 | Panes have no scrollback. The mouse works (clicks, border drags, wheel for programs that take it). | The wheel does nothing over a plain shell, and you can't scroll up inside a pane; use the program's own pager or `hc read`. | Keep a scrollback in the pane's parser, then let the wheel scroll it. |
| 3 | A restored terminal runs its command again from the start. | Unsaved state and screen history of the old program are gone. | None: herdr-core can't bring a stopped program back. |
| 4 | A second client attached to a workspace pane (for example `hc attach`) takes the size: herdr-core gives it to the last active viewer. | The program reflows to the smaller size. | Observe-only stream until the first key. |
| 5 | The Windows toast (WSL) is sent with exit code 0, but nobody has confirmed it appears. | Desktop notifications may be silent on WSL; the bell still rings. | Confirm on a real desktop. |
| 6 | No release binaries and no AUR entry. The PKGBUILD builds the local checkout. | Install from source. | Tag `v0.3.0`, point the PKGBUILD at the tarball. |
| 7 | Windows: not tested in a real console (Windows Terminal, conhost); only inside an hc terminal. Bracketed paste may be refused by the console (a paste then arrives as typed keys). | Pastes into panes can be slower, and a multi-line paste may run line by line. | Check in Windows Terminal. |
| 8 | Windows: the `socket` key of herdr-core's `config.toml` is not read, and win32-input-mode (`CSI ?9001h`) is not implemented (plain VT keys work in PowerShell 7 and cmd.exe). | A program that insists on win32-input-mode may misread keys. | Encode keys that way when a pane asks. |

Fixed:

- ✅ When neovim scrolled a window next to others (the main buffer beside a dap-ui sidebar), the windows beside it were wiped: neovim scrolls with left/right margins (`CSI ?69h`, `CSI Pl;Pr s`), which the vt100 crate ignored. Hiero now carries a vt100 fork that supports them.
- ✅ With a pane streaming fast output (nvim plugins like dap-ui), the screen could fall seconds behind and show old frames mixed with new ones, worst on Windows where output goes through ConPTY. Hiero now applies all queued output before drawing and draws at most every 16 ms while busy.
- ✅ After a core restart, panes stayed `ended` until Enter or a tab switch (W5: they restore by themselves).

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo run -p hiero                     # the workspace
```

The test setup that doesn't touch your own core: start a separate session with
`HC_SESSION=test hc server start`, and prefix every `hc` and `hiero` command with `HC_SESSION=test`.

### Development history

The per-phase checklists are in [docs/HISTORY.md](docs/HISTORY.md). Phases 0 to 6 built the agent
board, which was removed in 0.3.0 (it lives in git history; the last commit with it is `49e17c5`).

| Phase | What |
|---|---|
| 0 | Foundation: `hc-client` library (JSON-RPC, events, reconnecting watcher) |
| 1 | Agent board: status, NEEDS YOU, preview, search, notifications |
| 2 | Drive agents: spawn, quick actions, attach, templates |
| 3 | Status bridge for Claude Code, codex, pi (`hiero report`, `hiero integrate`) |
| 4 | Tasks and git worktrees: review, merge, pull request |
| 5 | Pipelines, broadcast, remote machines |
| 6 | Usage panel, config file, remembered state, log file, Arch package |
| W1 | Workspace data model in doc `hiero-ws`, `hiero ws` CLI |
| W2 | Workspace screen: sidebar, prefix key, one live pane |
| W3 | Tabs and splits |
| W4 | Restore on open, delete with terminals, menu of the old views |
| W5 | Core-restart recovery, menu views as overlays, sidebar hide and resize, README, 0.2.0 |
| 0.3.0 | Removed the agent board and its tools; Hiero is only the workspace |

Still open: release binaries for other distros (needs hosted releases and CI), themes (colours
are fixed).

## Source layout

```
crates/
├── vt100/            fork of vt100 0.18.1 (MIT) with DEC left/right margins (DECLRMM/DECSLRM)
├── hc-client/src/
│   ├── lib.rs        exports
│   ├── endpoint.rs   find the socket or pipe
│   ├── transport.rs  Unix socket / Windows named pipe
│   ├── client.rs     JSON-RPC calls
│   ├── error.rs      typed errors
│   ├── types.rs      Terminal, Status, RunParams, …
│   ├── events.rs     subscribe, notifications, Watcher
│   ├── framed.rs     framed mode: AttachStream
│   └── state.rs      TerminalSet
└── hiero/src/
    ├── main.rs       CLI entry: workspace, ws, config, debug
    ├── ws_tui.rs     workspace screen: event loop, starts and attaches terminals
    ├── ws_app.rs     workspace screen state: sidebar, prefix key, dialogs, restore (tested, no I/O)
    ├── ws_ui.rs      workspace screen drawing
    ├── workspace.rs  projects, workspaces, tabs, split layouts in doc "hiero-ws"
    ├── widgets.rs    text input, popups, painting a terminal screen
    ├── keys.rs       key and mouse events → terminal bytes
    ├── config.rs     config.toml, remembered UI state
    ├── paths.rs      config/state folders, home, folder spelling (Unix and Windows)
    └── notify.rs     bell and desktop notifications
packaging/arch/       PKGBUILD
docs/HISTORY.md       per-phase checklists
```

## License

Apache-2.0
