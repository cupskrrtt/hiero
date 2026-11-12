# hiero

A terminal workspace on top of [herdr-core](https://github.com/herdrdev/herdr-core) (`hc`).

The sidebar lists your projects, and each project holds named workspaces (`frontend`,
`backend`, `runner`, …). A workspace is a set of tabs and splits, and every pane is a real
terminal (nvim, Claude Code, a shell, anything) that herdr-core keeps running in the background.
Quit Hiero, come back later, and the layout and the programs are still there.

```
┌ projects ──────────┐  1 editor   2 agents   3 shell
│▾ shosha            │┏ nvim . t4 ━━━━━━━━━━━━━━━━━━━┓┌ claude t5 ─────────────────┐
│  ▶ ● frontend      │┃                              ┃│                            │
│    ◐ backend       │┃  fn main() {                 ┃│                            │
│    ✓ docs          │┃                              ┃└────────────────────────────┘
│▾ hiero             │┃                              ┃┌ server t6 ─────────────────┐
│    · notes         │┃                              ┃│ $ cargo run                │
└────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛└────────────────────────────┘
 t/T tab · v/s split (V/S ask) · hjkl move · HJKL size · 1-9 [ ] tab · … · ? help
```

- **Persistent.** Panes are herdr-core terminals. Closing Hiero, switching workspaces or losing the
  connection doesn't stop them.
- **Restored.** Layouts are saved. When a workspace opens, panes whose terminals are gone start
  again with their saved command.
- **Out of the way.** Hiero's own keys sit behind a prefix key (`ctrl+b`), so nvim and Claude get
  every other key. The mouse works too.
- **Status at a glance.** The sidebar shows which workspace is working, done, or needs you, from
  what the programs report (Claude Code does this by itself).

Hiero keeps no database of its own: terminals live in herdr-core, and the projects, workspaces
and layouts live in herdr-core's document store. It runs on Linux, WSL and native Windows.

## Install

You need herdr-core installed and running (`hc server status`).

```sh
# Linux / WSL / Windows, with Rust
cargo install --locked --git https://github.com/cupskrrtt/hiero hiero

# a fixed release
cargo install --locked --git https://github.com/cupskrrtt/hiero --tag v0.3.1 hiero

# from a checkout
cargo install --locked --path crates/hiero

# Arch: build and install a package from a checkout
cd packaging/arch && makepkg -si
```

Update with the same `cargo install` command plus `--force`. On Windows, quit Hiero first: a running
`.exe` can't be replaced.

**Windows** needs herdr-core for Windows (`hc.exe`) and Rust with the MSVC toolchain
(`stable-x86_64-pc-windows-msvc`) plus the Visual Studio C++ build tools. See [Windows](#windows).

## Quick start

1. Run `hiero`. The sidebar is empty and has the focus.
2. `A` adds a project: a name and a folder.
3. `a` adds a workspace to it (its folder defaults to the project's). It opens with a shell.
4. `ctrl+b v` splits side by side, `ctrl+b s` stacked. `ctrl+b V` / `S` ask for a command first:
   type `nvim .` or `claude`. `ctrl+b t` opens a new tab (`T` asks for a command).
5. `ctrl+b h` `j` `k` `l` moves between panes; or click one. `ctrl+b ?` shows every key.
6. `ctrl+b q` quits Hiero. The programs keep running.
7. Run `hiero` again: the same workspace opens and every pane reattaches to its terminal.

## Keys

### After the prefix (`ctrl+b`)

The bottom bar shows `prefix` until the next key.

| Key | Action |
|---|---|
| `t` / `T` | new tab with a shell / ask for a command first |
| `v` / `s` | split side by side / stacked: a shell in the same folder, which gets the focus |
| `V` / `S` | the same, asking for a command first |
| `h` `j` `k` `l`, arrows | move to the pane on the left / below / above / right |
| `H` `J` `K` `L` | narrower / taller / shorter / wider (5% a press) |
| `1` … `9`, `[` / `]` | go to tab N, previous / next tab |
| `r` / `R` | rename the tab / the pane (an empty pane name brings back the automatic title) |
| `x` / `X` | close the pane / the tab; asks first, because its programs end |
| `z` | zoom the pane to the whole area; again to undo (not saved) |
| `c` / `PgUp` | [copy mode](#copy-mode): scroll back through the pane, select and copy (`PgUp` starts a page up) |
| `w` | switch between sidebar and workspace (shows a hidden sidebar) |
| `b` | hide or show the sidebar |
| `<` / `>` | narrower / wider sidebar (4 columns a step, 16 to 60) |
| `n` / `N` | new workspace in this project / new project |
| `?` | help |
| `q` | quit Hiero; the terminals keep running |
| `ctrl+b` | send `ctrl+b` itself to the program |
| `Esc` | cancel |

The last pane of a tab closes the tab. The last tab of a workspace can't be closed: delete the
workspace instead.

### In the sidebar (no prefix)

| Key | Action |
|---|---|
| `j` `k`, `↑` `↓`, `g` `G`, `Home` `End` | move |
| `Enter` | open a workspace; on a project, fold or unfold it |
| `a` / `A` | add a workspace to this project / add a project |
| `r` | rename |
| `D` | delete; asks: `y` end its terminals · `k` keep them running · `n` cancel |
| `J` / `K` | move down / up |
| `z` | fold or unfold the project |
| `Esc`, `l`, `→`, `Tab` | back to the workspace |
| `?` / `q` | help / quit |

### Mouse

| Mouse | Action |
|---|---|
| click a workspace / a project | open it / fold or unfold it |
| click a tab | go to it |
| click a pane | focus it |
| drag the line between two panes | resize the split (saved when you let go) |
| wheel over the sidebar | move the selection |
| wheel over a pane | goes to a program that takes the mouse; arrow keys for a full-screen one (less, man); up over a shell scrolls its history in copy mode, and scrolling back to the bottom leaves it |
| click / drag in copy mode | place the cursor / select |

When the program in a pane turns on mouse reporting (nvim with `mouse=a`, htop, tmux), its clicks,
drags and wheel go to it, at positions inside the pane. Hold **Shift** while dragging to select text
with your terminal's own selection. `mouse = false` under `[workspace]` leaves the mouse to the
terminal.

### Copy mode

`ctrl+b c` (or `ctrl+b PgUp`, or the wheel over a shell) freezes the pane and lets you move through
its history: what scrolled off the top, up to `scrollback` lines. The pane's title shows
`copy · N/M up`. Output that arrives meanwhile is kept but doesn't move what you're reading.

| Key | Action |
|---|---|
| `h` `j` `k` `l`, arrows | move the cursor (the view follows) |
| `w` / `b` | next / previous word |
| `0` `^` `$` | start of the line / first text / end |
| `H` `M` `L` | top / middle / bottom of the view |
| `ctrl+u` / `ctrl+d` | half a page up / down |
| `ctrl+b` `ctrl+f`, `PgUp` `PgDn` | a page up / down |
| `ctrl+y` / `ctrl+e` | the view a line up / down |
| `g` / `G` | the oldest line / back to the bottom |
| `v` / `V` (or `Space`) | select characters / whole lines; again to stop |
| `y`, `Enter` | copy the selection (or the cursor's line) and leave |
| `/` / `?` | search down / up; lowercase matches either case |
| `n` / `N` | next / previous match |
| `Esc` | drop the selection; with none, leave |
| `q`, `ctrl+c` | leave |

Copying sends OSC 52 to your terminal (Alacritty, kitty, WezTerm, foot, iTerm2 and Windows Terminal
accept it; tmux needs `set -g set-clipboard on`). On Windows and WSL Hiero also hands the text to
`clip.exe`, so it lands even where a console drops OSC 52. Lines that wrapped in the pane come
out joined.

History lives in two places: Hiero keeps it per pane, and asks herdr-core for it whenever it
attaches. So a pane has its history back after you quit and reopen Hiero, after a resize, and after
a core restart (for a terminal that kept running). Full-screen programs (nvim, less, htop) draw on
the alternate screen, which has no history; copy mode there works on what is shown.

## How it works

**Projects and workspaces.** A project has a root folder; a workspace has its own folder (the
project's by default). Names are unique among siblings. Rename, reorder and delete them in the
sidebar or with `hiero ws …`; changes from the command line show up live.

**Panes.** Every pane is an hc terminal, labelled `hiero.ws`, `hiero.pane` and `hiero.project`, with
`HIERO_PANE` and `HIERO_WS` in its environment. The focused pane has a thick border and the cursor;
keys and paste go to it only. A pane opened with a command stores the command, so it can be started
again. Its title shows its name, else its command (or `shell`), then the terminal address.

**Layouts** — tabs, splits, sizes, pane names and commands — are stored in herdr-core's document
`hiero-ws`. The open workspace and the sidebar's width and visibility are remembered per machine.

**Restore.** When a workspace opens, every pane in every tab gets a running terminal; only the
visible tab is drawn. A pane starts again by itself (in its folder, with its command or a shell)
when its terminal is gone from herdr-core, or finished because the system stopped it
(`server_stop`, `host_shutdown`, `holder_lost`, `stopped_by_restart`). If its program exited or you
killed it, the pane shows `ended: …` and `Enter` starts a fresh one. Nothing starts twice.
`restore = false` turns the automatic start off: those panes say "not running" until `Enter`.

**Core restart while Hiero is open.** A red banner shows while herdr-core is unreachable. When it's
back, Hiero applies the same rule to every pane and says `core restarted — restored N panes`.

**Status dots.** `●` working, `◐` needs you (or an error), `✓` done, `○` idle, `·` nothing reported.
Shown per workspace, and on a folded project for its workspaces. The bottom bar names a workspace
that needs you, also when the sidebar is hidden. A workspace turning "needs you" or "done" rings the
bell and sends a desktop notification (see `[notify]`).

**Delete.** `D` asks what happens to the terminals: `y` ends them, `k` leaves them running (they are
no longer in any workspace), `n` cancels.

**Nesting.** Running `hiero` inside one of its own panes stops with *You're already inside Hiero
(shosha › backend)…*; use `ctrl+b w` instead. `hiero --nested` opens it anyway, and even then Hiero
never shows the terminal it runs in: that pane says `ended: this is Hiero itself` rather than drawing
itself forever. `hiero ws`, `hiero config` and `hiero debug` work normally inside panes.

## Keeping terminals alive

herdr-core keeps the terminals; Hiero only shows them. What can still end them:

- **Linux and WSL:** with systemd, the core stops when your **last login session ends** — on WSL,
  when you close the last WSL window. Keep it running with:
  ```sh
  sudo loginctl enable-linger $USER
  ```
- **Windows:** the core survives closing windows, sleep and locking the screen. It ends when you
  **sign out**.
- **Everywhere:** a reboot, `wsl --shutdown`, or herdr-core crashing.

After that, opening a workspace brings the layout back and starts every pane again from its command.
The old programs' state and screens are gone. For Claude, open the pane with the command
`claude --continue` (`ctrl+b T`, `V` or `S`): after a restart it resumes the last conversation in that
folder instead of starting a new one.

## Windows

Hiero runs natively on Windows as well as in WSL.

- **Two separate worlds.** Native Windows Hiero and Hiero in WSL talk to two different herdr-core
  servers, with separate terminals and separate workspaces.
- Hiero finds the core on the named pipe `\\.\pipe\herdr-core-<user>-<session>.sock` (`<user>` is
  `%USERNAME%` with letters and digits only, `<session>` is `HC_SESSION` or `default`). `HC_SOCKET`
  overrides it.
- A pane without a command starts PowerShell 7, or Windows PowerShell (herdr-core's choice).
- Folders are stored as plain `C:\…` paths, and `~` means `%USERPROFILE%`.
- Notifications are a Windows toast, plus the bell.

## Configuration

`~/.config/hiero/config.toml` (Windows: `%APPDATA%\hiero\config.toml`). Everything has a default.
`hiero config init` writes a commented file, `hiero config show` checks it and prints what is in
effect. A broken file is reported on the bottom bar and ignored.

```toml
[workspace]
prefix = "ctrl+b"        # ctrl+<letter> (not c, h, i, j, m) or ctrl+space
restore = true           # start missing terminals when a workspace opens
mouse = true             # clicks, wheel, border drags; Shift+drag still selects text
scrollback = 5000        # lines of history per pane, for copy mode; 0 turns it off

[notify]
bell = true
desktop = true           # notify-send on Linux; a Windows toast on Windows and WSL
on = ["blocked", "done"] # blocked: a workspace needs you (or a program errored); done
```

Sections left over from old versions (`[board]`, `[agents]`, `[keys]`) are skipped with a warning.

## Command line

```sh
hiero                                    # the workspace
hiero --nested                           # the workspace, even inside a Hiero pane

hiero ws list                            # projects › workspaces › tabs › panes
hiero ws add-project shosha --root ~/code/shosha        # root defaults to the current folder
hiero ws add shosha frontend --folder ~/code/shosha/web # folder defaults to the project's
hiero ws rename shosha frontend web      # a workspace; `rename shosha new-name` for a project
hiero ws move shosha web up              # or down; `move shosha up` for a project
hiero ws rm shosha web                   # `rm shosha` removes the project

hiero config [path | init | show]

hiero debug status                       # connect and show the core
hiero debug ls [--all]                   # terminals
hiero debug read t1 [--lines 50]         # a terminal's screen
hiero debug events [--raw]               # follow live events
hiero debug keys                         # what your terminal sends; marks the prefix
hiero debug type t1 'ls\r'               # type into a terminal (\r \n \t \e \\ \xNN)
hiero debug doc --name hiero-ws [JSON]   # read or write a herdr-core document
hiero debug vtcompare t4 [--secs 20] [--dump FILE]   # check Hiero's screen against herdr-core's
```

Projects and workspaces are named exactly; folders must exist.

## Troubleshooting

- **The prefix does nothing.** Run `hiero debug keys` and press it. If nothing shows up, something in
  front of Hiero takes the key (tmux uses `ctrl+b` too; `ctrl+space` is often an input-method
  switch). Pick another: `prefix = "ctrl+a"`.
- **A pane draws wrong.** Run `hiero debug vtcompare <terminal>` in another terminal while it happens
  (the address is in the pane's title). Differing rows mean Hiero's emulator got something wrong, and
  the report shows which escape sequences it didn't handle. Zero rows means the problem is between
  Hiero and your outer terminal.
- **Terminals disappear when you close your windows.** See [Keeping terminals alive](#keeping-terminals-alive).
- **Start over.** `hiero debug doc null --name hiero-ws` forgets every project and layout. Running
  terminals are not touched.

## Files and environment

| Path | What |
|---|---|
| `~/.config/hiero/config.toml` | settings (Windows `%APPDATA%\hiero\`) |
| `~/.local/state/hiero/ui.json` | open workspace, sidebar width and visibility (Windows `%LOCALAPPDATA%\hiero\`) |
| `~/.local/state/hiero/hiero.log` | log (rotated past 5 MB) |
| herdr-core document `hiero-ws` | projects, workspaces, tabs, splits, pane names, commands and terminals |

| Variable | Meaning |
|---|---|
| `HC_SOCKET` | the core's socket (on Windows a pipe name) |
| `HC_SESSION` | a separate herdr-core session (default `default`) |
| `HIERO_LOG` | log level, e.g. `debug` |
| `HIERO_NO_NOTIFY` | no desktop notifications (the bell still rings) |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | override the config and state folders (also on Windows) |
| `HIERO_PANE`, `HIERO_WS` | set by Hiero inside its panes |

## Known issues

| # | Issue | Impact |
|---|---|---|
| 1 | A program shows `·` until it reports a status to herdr-core (OSC 7501), e.g. while Claude waits at its "trust this folder?" dialog. Claude Code 2.1.319+ reports by itself; other agents may not at all. | A new pane can look idle while it waits for you. |
| 2 | A restored pane runs its command again from the start. | The old program's state and screen are gone (see `claude --continue` above). |
| 3 | A second client attached to a pane's terminal (e.g. `hc attach`) takes its size: herdr-core sizes a terminal for the last active viewer. | The program reflows until Hiero's pane is used again. |
| 4 | Hiero doesn't honour synchronized output (`CSI ?2026`). | During heavy redraws you may glimpse a half-drawn frame for a moment; it settles. |
| 5 | Windows: tried day to day in Alacritty; not in Windows Terminal or conhost. Bracketed paste may be refused by a console. win32-input-mode (`CSI ?9001h`) isn't implemented, and herdr-core's `socket` config key isn't read. | A multi-line paste may arrive as typed lines; a program that insists on win32 key encoding may misread keys. |
| 6 | The desktop notification on WSL (a Windows toast) has never been confirmed on screen. | It may be silent; the bell still rings. |
| 7 | No prebuilt binaries and no AUR entry; the PKGBUILD builds a local checkout. | Install from source. |

Fixed recently: panes have scrollback and a [copy mode](#copy-mode); neovim's left/right-margin scrolling (the main buffer beside a dap-ui sidebar used
to wipe the sidebar) — Hiero carries a vt100 fork that supports margins; a pane streaming fast
output no longer makes the screen fall behind; a resized pane takes its screen again from
herdr-core (a pane that grew used to show its text on the wrong rows); and panes stay attached
while another tab or workspace is shown, so their programs keep the pane's size instead of
printing for herdr-core's 200x120.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo run -p hiero
```

Test against a separate herdr-core session so your own terminals and workspaces are left alone:
`HC_SESSION=test hc server start`, prefix every `hc` and `hiero` command with `HC_SESSION=test`, and
`HC_SESSION=test hc server stop` when done. `hiero debug type` drives a running Hiero, and
`hiero debug vtcompare` checks drawing.

```
crates/
├── vt100/            fork of vt100 0.18.1 (MIT) with DEC left/right margins
├── hc-client/src/    herdr-core client
│   ├── endpoint.rs   find the socket or pipe
│   ├── transport.rs  Unix socket / Windows named pipe
│   ├── client.rs     JSON-RPC calls
│   ├── framed.rs     attach streams
│   ├── events.rs     events and the reconnecting Watcher
│   ├── state.rs      the live terminal list
│   ├── types.rs      Terminal, Status, RunParams, …
│   └── error.rs      typed errors
└── hiero/src/
    ├── main.rs       command line
    ├── ws_tui.rs     event loop: keys, mouse, streams, drawing
    ├── ws_app.rs     state: sidebar, prefix, dialogs, restore (tested, no I/O)
    ├── ws_ui.rs      drawing
    ├── workspace.rs  projects, workspaces, tabs, splits (doc "hiero-ws")
    ├── widgets.rs    text input, popups, painting a terminal screen
    ├── keys.rs       keys and mouse → terminal bytes
    ├── config.rs     config.toml, remembered state
    ├── paths.rs      folders and paths on Unix and Windows
    └── notify.rs     bell and desktop notifications
packaging/arch/       PKGBUILD
docs/HISTORY.md       how Hiero got here
```

Hiero started as an agent dashboard (phases 0–6) and became a workspace (W1–W5); the dashboard
was removed in 0.3.0 and lives in git history (last commit with it: `49e17c5`). Details in
[docs/HISTORY.md](docs/HISTORY.md).

## License

Apache-2.0
