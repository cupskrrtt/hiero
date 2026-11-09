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
 ctrl+b then  t tab · v s split · hjkl move · z zoom · m menu · ? help · q quit
```

Hiero keeps no database of its own. Terminals live in herdr-core, and Hiero's data (projects,
workspaces, layouts, tasks) lives in herdr-core's document store.

Hiero's own keys sit behind a prefix key (default `ctrl+b`), so nvim and Claude get every other
key. The older agent tools (board, tasks in git worktrees, pipelines, broadcast, usage,
templates) are one menu away (`ctrl+b m`), or run `hiero board`.

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
| `x` / `X` | close the pane / the tab (asks; its programs end) |
| `z` | zoom the pane to the whole area, again to undo (a view only, not saved) |
| `w` | switch between sidebar and workspace (shows a hidden sidebar) |
| `b` | hide or show the sidebar |
| `<` / `>` | narrower / wider sidebar (4 columns a step, 16 to 60) |
| `n` / `N` | new workspace in this project / new project |
| `m` | menu of the agent tools |
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

The menu (`prefix m`, then `j` `k` `Enter`) lists Agents board, Tasks, Pipelines, Usage, Templates
and Broadcast. A choice opens that view over the workspace. Actions inside the view (forget a task,
launch a pipeline) keep you in it. `q` or `Esc` from the view returns to the workspace, with every
pane attached again. Broadcast with no live agents just says so and stays in the workspace.

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

**Delete.** `D` on a workspace or project asks what to do with its terminals: `y` ends them and
deletes, `k` deletes and leaves them running, `n` or `Esc` cancels.

**Status dots.** The sidebar shows `●` working, `◐` needs you, `✓` done, `○` idle, `·` no terminal,
per workspace and on a collapsed project. The bottom bar names a workspace that needs you, also
when the sidebar is hidden. Changes to needs-you or done ring the bell and notify, following the
`[notify]` rules.

**`[workspace]` config.** `prefix` is `ctrl+<letter>` (not c, h, i, j, m) or `ctrl+space`.
`restore = false` turns the automatic start off: panes without a terminal then say "not running"
and `Enter` starts one.

## Agent tools

These came first and are still there. Open them from the menu (`ctrl+b m`), or run `hiero board`
for the board on its own screen.

- **Board.** Every terminal at a glance: name, agent, status, folder, branch, runtime, title. A
  NEEDS YOU section on top lists blocked, done or failed agents. Preview of the selected agent,
  fuzzy search (`/`), grouping by project or as a tree. Start (`c`), send (`s`), interrupt, kill,
  rename, attach full-screen (`a`) or inside the board (`o`).
- **Tasks and worktrees (opt-in).** The new-agent form has a worktree switch (off by default).
  When on, Hiero makes `../<repo>.hiero/<slug>` on branch `hiero/<slug>` and starts the agent
  there. Review the diff (`v`), merge (`m`), open a pull request (`P`), keep or discard. The task
  list is `T`.
- **Pipelines** (`w`). TOML files of stages in `~/.config/hiero/pipelines/`, plus a built-in
  `plan-build-review`. A stage is an agent with a prompt or a plain command; `count = N` runs N
  members side by side; `worktree = true` gives each its own branch. The next stage starts when
  every member's turn has ended.
- **Broadcast** (`B`). One message to every live agent shown. From a shell:
  `hiero broadcast [--label k=v]… [--force] <text>`.
- **Usage** (`u`). Claude tokens per agent, read from its transcript in `~/.claude/projects`.
  Tokens only, no prices.
- **Templates** (`t`). herdr-core templates: list, new, edit in `$VISUAL`/`$EDITOR`, delete, run.
- **Status bridge.** herdr-core only knows a status that a program reports through OSC 7501.
  Claude Code 2.1.319 and later reports natively. For older versions and other agents:
  `hiero integrate claude` installs hooks in `~/.claude/settings.json` (it declines on versions
  that report natively unless `--force`), `hiero integrate codex` prints the Codex `notify` line,
  and `hiero report <state> [--kind] [--msg]` reports from any script or hook.

### Board keys

| Key | Action |
|---|---|
| `j` `k` / `↑` `↓`, `g` `G` | select, first / last |
| `n` / `Tab` | next agent that needs you |
| `x` | dismiss from NEEDS YOU (comes back on its next status change) |
| `/` | fuzzy search; `Esc` clears |
| `p` | grouping: project → tree → flat |
| `f` | show finished terminals on/off |
| `PgUp` `PgDn` / `Ctrl+U` `Ctrl+D` | scroll the preview |
| `r` | refresh the preview |
| `c` | new agent |
| `s` | send a message (`Ctrl+F` forces it while the agent is blocked) |
| `a` / `Enter` | attach full-screen; `Ctrl+\` (or `Ctrl+]`) comes back |
| `o` | open the agent inside the board; `Ctrl+\` comes back |
| `i` | interrupt |
| `K` | kill (asks first) |
| `d` | remove a finished terminal |
| `e` | keep on/off (a lost program is replaced by a shell) |
| `R` | rename |
| `t` | templates: `r` run · `e` edit · `n` new · `D` delete |
| `v` | review this agent's task: `j`/`k` file · `m` merge · `P` pull request · `K` keep · `D` discard |
| `T` | task list: `Enter` review · `g` go to agent · `D` forget a finished task |
| `w` | pipelines: `Enter` launch / go to run · `e` edit · `S` stop run · `A` adopt run · `D` forget run |
| `B` | broadcast to every live agent shown |
| `u` | Claude token usage per agent |
| `?` | help |
| `q` / `Ctrl+C` | quit |

Status symbols: `●` working · `◐` blocked · `✓` done · `✗` error · `○` idle · `·` no status ·
`■` finished.

## Configuration

`~/.config/hiero/config.toml`. Every setting has a default, so the file can stay short.
`hiero config init` writes a commented starting file, `hiero config show` checks it and prints what
is in effect. A broken file is reported and ignored.

```toml
[board]
grouping = "project"         # project, tree or flat (p cycles)
show_finished = true         # f toggles
preview_history = 200        # lines above the screen in the preview
remember = true              # restore the open workspace, sidebar, grouping and selection

[workspace]
prefix = "ctrl+b"            # ctrl+<letter> (not c, h, i, j, m) or ctrl+space
restore = true               # start missing terminals when a workspace opens

[notify]
bell = true
desktop = true               # notify-send, or a Windows toast on WSL
on = ["blocked", "error", "done", "failed", "pipeline"]   # also: exited

[agents.claude]
command = "claude --model opus"

[agents.pi]
task = "send"                # type the task in after start; default "arg" passes it as the last argument

[keys]                       # remap board keys: action = "key"
kill = "X"                   # the old key (K) stops working
```

## CLI reference

```sh
hiero                                  # the workspace
hiero board                            # the agent board on its own screen

# workspaces (the same edits are in the UI)
hiero ws add-project shosha --root ~/code/shosha     # root defaults to the current folder
hiero ws add shosha frontend --folder ~/code/shosha/web   # folder defaults to the project root
hiero ws list                          # projects > workspaces > tabs > panes
hiero ws rename shosha frontend web    # or: rename shosha shosha-app
hiero ws move shosha web up            # or: move shosha down
hiero ws rm shosha web                 # or: rm shosha

hiero broadcast --label hiero.project=shosha "please rebase on main"
hiero report blocked --kind question --msg "pick a database"   # from inside an hc terminal
hiero integrate claude [--status | --uninstall | --force]
hiero integrate codex                  # prints the Codex notify line
hiero config [path | init | show]

hiero debug status                     # handshake and core info
hiero debug ls --all                   # terminals
hiero debug events                     # follow live events
hiero debug read t1 --lines 50         # a terminal's screen
hiero debug keys                       # what your terminal sends (to check a prefix key)
hiero debug type t1 'ls\r'             # type raw bytes over an attach stream (\r \n \t \e \xNN)
hiero debug doc [JSON] [--name hiero-ws]   # read or write a document
```

Projects and workspaces are named exactly on the command line. Folders must exist.

## Environment variables

| Variable | Meaning |
|---|---|
| `HC_SOCKET` | Use this core socket |
| `HC_SESSION` | Use a separate hc session (default `default`) |
| `HIERO_LOG` | Log level, for example `debug` or `hc_client=trace` |
| `HIERO_NO_NOTIFY` | Turn off desktop notifications (the bell still rings) |
| `VISUAL` / `EDITOR` | Editor for templates and pipelines (default `vi`) |
| `XDG_CONFIG_HOME` | Where `hiero/config.toml` and `hiero/pipelines/` live (default `~/.config`) |
| `XDG_STATE_HOME` | Where `hiero/hiero.log` and `hiero/ui.json` live (default `~/.local/state`) |

## Files

| Path | What |
|---|---|
| `~/.config/hiero/config.toml` | settings |
| `~/.config/hiero/pipelines/*.toml` | pipeline definitions |
| `~/.local/state/hiero/ui.json` | remembered: open workspace, sidebar width and state, board filters |
| `~/.local/state/hiero/hiero.log` | log of the UI (rotated past 5 MB) |
| herdr-core doc `hiero-ws` | projects, workspaces, tabs, splits, pane commands and terminals |
| herdr-core doc `hiero` | tasks and pipeline runs |

Reset the workspaces with `hiero debug doc null --name hiero-ws`. This forgets every layout;
running terminals are not touched.

## Known issues

| # | Issue | Impact | Plan |
|---|---|---|---|
| 8 | Before its first report an agent shows `·`, including while Claude waits at its "trust this folder?" dialog. | A new agent in a new folder looks idle while it waits for you. | Show "starting / not reported" for young agents. |
| 9 | Codex (`hiero hook codex`) and pi are untested; neither is installed here. Codex `notify` only reports finished turns. | Partial status for Codex; none for pi unless its hooks call `hiero report`. | Test when installed. |
| 18 | Panes have no scrollback and no mouse support. | You can't scroll up inside a pane; use the program's own pager or `hc read`. | Keep a scrollback in the pane's parser, then add mouse. |
| 19 | A restored terminal runs its command again from the start. | Unsaved state and screen history of the old program are gone. | None: herdr-core can't bring a stopped program back. |
| 20 | The prefix menu's Broadcast only counts live terminals that report a status or were started as an agent, but the board's broadcast prompt sends to every live terminal in its list, shells included. | Broadcast from the menu can type into a plain shell. | Make the board skip terminals that are not agents. |
| 3 | Opening an agent inside the board (`o`) resizes it to the pane. herdr-core gives the size to the last active viewer. This also applies to any second client attached to a workspace pane. | The program reflows to the smaller size. | Observe-only stream until the first key. |
| 4 | The board redraws timers and the spinner every second. | When the board runs inside an hc terminal, `hc send` into it fails with "output has not settled". The workspace screen is not affected. | Redraw only when something visible changes. |
| 6 | The Windows toast (WSL) is sent with exit code 0, but nobody has confirmed it appears. | Desktop notifications may be silent on WSL; the bell still rings. | Confirm on a real desktop. |
| 7 | Rename on the board is stored as a `hiero.name` label because hc has no rename. | `hc ls` still shows the original name. | Use a real rename if herdr-core adds one. |
| 10 | Pull requests (`P`) are only tested up to the push failing cleanly; `gh pr create` is untested. | The PR step may need fixes on first use. | Test against a real GitHub repo. |
| 11 | Merge needs the main tree on the task's base branch with no uncommitted changes, and stops at conflicts. | You resolve conflicts by hand, then merge again. | A "rebase onto base" action. |
| 12 | Every running Hiero board writes timeline notes, so two boards open at once record each note twice. | Duplicate timeline lines. | A lease in the doc. |
| 13 | New files show in review only up to 500 lines; binary files show as "(binary file)". | Large generated files are cut short. | Open the worktree for the rest. |
| 14 | A pipeline only moves forward while the board that drives it is open. | Close it and the run pauses; reopen and press `A` on the run. | Adopt runs automatically. |
| 15 | The built-in `plan-build-review` (Claude) is untested end to end; only a command pipeline ran. Agents that never report status (plain shells) never finish a stage. | A real Claude pipeline may need prompt tuning. | Run it on a real task. |
| 16 | Agents on other machines show no git branch and can't use worktrees. | Less information for remote agents. | Read git facts through `hc exec --on`. |
| 17 | No release binaries and no AUR entry. The PKGBUILD builds the local checkout. | Install from source. | Tag `v0.2.0`, point the PKGBUILD at the tarball. |

Fixed:

- ✅ After a core restart, panes stayed `ended` until Enter or a tab switch (W5: they restore by themselves).
- ✅ Actions inside a menu view (forget a task, launch a pipeline) closed the view (W5).
- ✅ Opening Broadcast from the menu said "no live agents" even when agents ran (W5: the board's list is built before the view opens).
- ✅ Agents on PATH always got the task as the last argument (Phase 6: `task = "send"`).
- ✅ The board logged to stderr, which drew over the screen (Phase 6: log file).
- ✅ Real coding agents showed `·` (Phase 3: Claude reports natively; hooks and `hiero report` cover the rest).
- ✅ Coming back from full-screen attach quit the board (Phase 2).
- ✅ The board could attach to its own terminal (Phase 2).

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

The per-phase checklists are in [docs/HISTORY.md](docs/HISTORY.md).

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

Still open: release binaries for other distros (needs hosted releases and CI), themes (colours
are fixed).

## Source layout

```
crates/
├── hc-client/src/
│   ├── lib.rs        exports
│   ├── endpoint.rs   find the socket
│   ├── client.rs     JSON-RPC calls
│   ├── error.rs      typed errors
│   ├── types.rs      Terminal, Status, RunParams, …
│   ├── events.rs     subscribe, notifications, Watcher
│   ├── framed.rs     framed mode: AttachStream
│   └── state.rs      TerminalSet
└── hiero/src/
    ├── main.rs       CLI entry
    ├── ws_tui.rs     workspace screen: event loop, starts and attaches terminals
    ├── ws_app.rs     workspace screen state: sidebar, prefix key, dialogs, restore (tested, no I/O)
    ├── ws_ui.rs      workspace screen drawing
    ├── workspace.rs  projects, workspaces, tabs, split layouts in doc "hiero-ws"
    ├── tui.rs        board event loop: keys, core events, screen reads, timers
    ├── app.rs        board state and behaviour (tested, no I/O)
    ├── ui.rs         drawing (board, dialogs, attached pane)
    ├── forms.rs      dialog state: text input, new agent, templates
    ├── attach.rs     full-screen attach
    ├── keys.rs       key events → terminal bytes
    ├── integrate.rs  status bridge: hooks, report, installers
    ├── tasks.rs      task records in doc "hiero"
    ├── worktree.rs   git worktrees: create, review, merge, PR, remove
    ├── pipeline.rs   pipeline specs, runs and the stage driver
    ├── config.rs     config.toml, key remaps, remembered UI state
    ├── usage.rs      Claude token usage from transcripts
    ├── git.rs        repo root and branch, read from .git
    └── notify.rs     bell and desktop notifications
packaging/arch/       PKGBUILD
docs/HISTORY.md       per-phase checklists
```

## License

Apache-2.0
