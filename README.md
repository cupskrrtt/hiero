# hiero

A TUI agent manager on top of [herdr-core](https://github.com/herdrdev/herdr-core) (`hc`).

herdr-core keeps terminals and coding agents alive in the background and exposes them over a
JSON-RPC socket, but draws nothing. Hiero is the cockpit: see every agent at a glance, know
which ones need you, start, drive and take over agents, and run parallel tasks safely in
git worktrees.

```
hiero (one binary)
├── hc-client     JSON-RPC over the Unix socket; plain mode for calls and events, framed for attach
├── state         live picture of all terminals (snapshot + events)
├── store         Hiero's data (tasks, projects) in hc doc "hiero"; survives restarts
├── ui            ratatui: board · preview · in-pane terminal · dialogs
└── integrate     status bridge: agent hooks → OSC 7501 (hiero hook / report)
```

Hiero keeps no database of its own: terminals live in herdr-core, and Hiero's data lives in
herdr-core's document store. Close Hiero, reopen it, and everything is still there.

## Status

| Phase | What | State |
|---|---|---|
| 0 | Foundation: `hc-client` library | ✅ done |
| 1 | Agent board (MVP) | ✅ done |
| 2 | Drive agents: spawn, actions, attach | ✅ done |
| 3 | Status bridge for Claude Code, codex, pi | ✅ done |
| 4 | Tasks and git worktrees | ✅ done |
| 5 | Orchestration: pipelines, broadcast, machines | ✅ done |
| 6 | Polish: usage, config, packaging | ✅ done |

## Development phases

### Phase 0: Foundation ✅
**Goal:** talk to herdr-core from Rust.

- [x] Cargo workspace: `crates/hc-client` (library, no UI) and `crates/hiero` (app)
- [x] Find the socket the way hc does (`HC_SOCKET`, `$XDG_RUNTIME_DIR/herdr-core/<session>.sock`, …)
- [x] JSON-RPC client: calls can run at once, replies matched by id, clean failure when the core goes away
- [x] Typed errors for every error kind (`not_found`, `conflict`, `unavailable`, …)
- [x] Typed wrappers: `hello`, `server.status`, `terminal.run/list/get/read/send/keys/interrupt/kill/rm/label/keep`, `templates.list`, `doc.get/set`
- [x] `events.subscribe` with every event type decoded
- [x] `Watcher`: reconnects with backoff, heartbeat timeout, fresh snapshot after reconnect or `missed`
- [x] `TerminalSet`: applies events to a snapshot to keep an up-to-date list of terminals
- [x] `hiero debug status | ls | read | events | doc`

**Done when:** `hiero debug events` prints live events while you use `hc` in another shell.

### Phase 1: Agent board (MVP) ✅
**Goal:** see every agent and what state it's in, at a glance.

- [x] Full-screen ratatui board: name, agent, status (● working / ◐ blocked / ✓ done / ✗ error / – none), cwd, branch, runtime, title
- [x] **Needs-you** section at the top: blocked, done or errored agents, sorted by how long they've waited
- [x] Preview pane: the selected agent's last ~50 lines, refreshed on change
- [x] Group or filter by project (cwd or the `hiero.project` label); fuzzy search with `/`
- [x] Desktop notification and terminal bell when an agent moves into blocked or done
- [x] Banner when disconnected from the core, with automatic recovery

**Done when:** you can watch 3–5 agents and the ones waiting for you stand out right away.

### Phase 2: Drive agents ✅
**Goal:** never need the `hc` CLI by hand.

- [x] New-agent dialog (`c`): template, agent on PATH or shell; folder, task, name. Labels `hiero.project`, `hiero.task`, `hiero.agent`
- [x] Quick actions: `s` send (`Ctrl+F` forces while blocked), `i` interrupt, `K` kill (asks first), `d` remove finished, `e` keep on/off, `R` rename (stored as label `hiero.name`; hc has no rename)
- [x] Full-screen attach (`a` / `Enter`): Hiero hands over the terminal; `Ctrl+\` comes back
- [x] Attach inside the board (`o`): framed attach plus `vt100`, credit-based flow control, resize, bracketed paste
- [x] Template manager (`t`): list, new from a skeleton, edit in `$VISUAL`/`$EDITOR`, delete, start
- [x] `hc-client::AttachStream` for framed mode; `hiero debug type` to script input
- [x] Won't attach to, kill or preview the terminal Hiero itself runs in

**Done when:** you can start, talk to, take over and clean up agents entirely inside Hiero.

### Phase 3: Status bridge ✅
**Goal:** real status for agents that don't report it themselves.

herdr-core only knows a status the program reports through OSC 7501. Testing with a real session
showed that **Claude Code 2.1.319 already reports natively** (`app=claude-code`): working with a
message, `blocked:permission` with `approve Write: …`, and done. The board shows it with no setup.
So the bridge covers older Claude versions and other agents:

- [x] `hiero hook claude <Event>`: Claude Code hooks → OSC 7501 on the agent's terminal
  (`UserPromptSubmit`/tool use → working, `PermissionRequest`/permission notification → blocked,
  `Stop` → done). It stays quiet when Claude reports natively, so the two never fight
- [x] `hiero integrate claude [--uninstall] [--status] [--settings F] [--force]`: merges the hooks
  into `~/.claude/settings.json`, keeps other hooks (such as herdr's), backs the file up first.
  It declines on Claude Code 2.1.319+ unless `--force`
- [x] `hiero hook codex`: Codex `notify` (turn complete → done, with the last message);
  `hiero integrate codex` prints the `config.toml` line
- [x] `hiero report <state> [--kind] [--msg]`: report from any agent's hooks or a script
- [x] Fallback when there's no controlling terminal: `hiero.status*` labels the board reads
- [x] Board shows `claude-code` as `claude`

**Done when:** Claude agents show accurate working, blocked and done states on the board. ✅
(verified with real Claude Code 2.1.319 sessions)

### Phase 4: Tasks and git worktrees ✅
**Goal:** run parallel agents on one repo without them colliding.

A [git worktree](https://git-scm.com/docs/git-worktree) is a second working folder for the
same repo with its own branch checked out, so each agent gets its own copy of the code but
everything shares one history.

- [x] New-agent form has a **worktree** switch (on by default inside a git repo): Hiero makes
  `../<repo>.hiero/<slug>` on branch `hiero/<slug>` from the repo's HEAD and starts the agent there
  (`slug-2`, `slug-3`… when taken; the worktree is removed again if the agent fails to start)
- [x] Task records in herdr-core doc `hiero`: title, prompt, agent, repo, base branch/commit, branch,
  worktree, terminal, status, timeline, PR URL. Retries on conflicting writes; unknown keys kept
- [x] Timeline: turn ends (`turn 2 ended: done`), blocked / error / exited, and every decision
- [x] **Review** (`v` on an agent, or `Enter` in the task list): files changed since the fork point,
  including uncommitted edits and new files; coloured diff; commits ahead; uncommitted marker
- [x] **Merge** (`m`): stops the agent, commits leftovers (`hiero: <title>`), `merge --no-ff` into the
  base branch, removes worktree, branch and the empty `<repo>.hiero/`. Refuses if the main tree is
  on another branch or has uncommitted changes; aborts cleanly on conflicts
- [x] **Pull request** (`P`): commits leftovers, pushes, `gh pr create`; keeps the worktree
- [x] **Keep** (`K`) and **discard** (`D`: stops the agent, deletes worktree and branch)
- [x] Task list (`T`): status, branch, agent, age; `g` jumps to the agent; `D` forgets finished tasks

**Done when:** you can start 3 tasks on one repo, review each diff, and merge or discard them. ✅
(tested from the board with shell agents and a real Claude agent: create → review → merge, discard,
PR error path, task list)

### Phase 5: Orchestration ✅
**Goal:** agents working together.

- [x] **Pipelines** (`w`): TOML files of stages in `~/.config/hiero/pipelines/`, plus a built-in
  `plan-build-review`. A stage is an agent with a prompt or a plain `command`; `count = N` runs
  N members side by side; `worktree = true` makes each member a Phase 4 task on its own branch.
  Hiero starts the next stage when every member's turn has ended or it has exited; a command
  that exits non-zero fails the run. Prompts can use `{task}` `{n}` `{count}` `{branches}`
  `{dirs}` `{folder}` `{run}` to hand work from one stage to the next
- [x] Runs are stored in the `hiero` doc with every stage's members; one board drives each run
  (`A` adopts a run whose board went away); `S` stops, `D` forgets; `e` edits a pipeline
  (the built-in is copied to a file first)
- [x] **Tree view**: `p` cycles project → tree → flat. Pipeline members are hc children of the
  run's first agent, with notes into the parent turned off and outliving it
- [x] **Broadcast**: `B` sends one message to every live agent shown (search narrows it);
  `hiero broadcast [--label k=v]… [--force] <text>` from the shell
- [x] **Remote machines**: a machine row in the new-agent form when hc knows other machines;
  remote agents show as `box/t3` with `box:` in the project column; attach works through the
  relay. Worktrees stay local only
- [x] Clean exits of pipeline members don't land in NEEDS YOU; failures do

**Done when:** tested from the board: a 3-stage pipeline (command → 2 parallel worktree members →
command) ran to the end and handed plan, branches and folders along; tree view; broadcast from
the board and the CLI; a shell on a second machine (`hc machine add --via`) started, attached
and typed into from the board.

### Phase 6: Polish ✅
- [x] **Usage panel** (`u`): Claude tokens per agent (messages, input, output, cache write, cache
  read) from its transcript in `~/.claude/projects`, counted once per message. Tokens only, no
  prices: plans bill differently, and a wrong price is worse than none
- [x] **Config** `~/.config/hiero/config.toml` (`hiero config init | path | show`): board defaults,
  which changes alert and how (bell / desktop), per-agent launch settings (`command`, and
  `task = arg | send | none`), key remapping under `[keys]`. Checked on start; a broken file is
  reported on the board and ignored
- [x] **Remembers** grouping, show-finished, search and the selected agent between runs
  (`~/.local/state/hiero/ui.json`; `remember = false` turns it off)
- [x] **Log file** `~/.local/state/hiero/hiero.log` for the board (it used to write to stderr,
  over the screen); rotated past 5 MB
- [x] **Packaging**: release profile (thin LTO, stripped), Apache-2.0 `LICENSE`, Arch
  `packaging/arch/PKGBUILD` (builds and tests the checkout; notes for publishing on the AUR)
- [ ] Release binaries for other distros (needs a hosted repository and CI)
- [ ] Themes (colours are fixed for now)

## Known issues

Tracked here until fixed. ✅ = fixed.

| # | Issue | Impact | Plan |
|---|---|---|---|
| 8 | Before its first report an agent shows `·`, including while Claude waits at its "trust this folder?" dialog, which comes before any hook or native report. | A new agent in a new folder looks idle while it actually waits for you. | Show "starting / not reported" for young agents, and hint to attach. |
| 9 | Codex (`hiero hook codex`) and pi are untested; neither is installed here. Codex `notify` only reports finished turns, so Codex shows done but never working or blocked. | Partial status for Codex; none for pi unless its hooks call `hiero report`. | Test when installed; check whether they speak OSC 7501 natively. |
| 3 | Opening an agent inside the board (`o`) resizes the agent to the pane. herdr-core gives the size to the last active viewer. | The agent reflows to a smaller size; full-screen attach (`a`) sizes it back. | Could use an observe-only stream for viewing and become a viewer only on the first keypress. |
| 4 | The board redraws timers and the spinner every second, so its screen never goes quiet. | When Hiero itself runs inside an hc terminal, `hc send` / `hc keys` into it fail with "output has not settled". Normal use is unaffected. | Redraw only when something visible changes, and tick wait timers less often. |
| 6 | The Windows toast (WSL) is sent with exit code 0, but nobody has confirmed it actually appears. | Desktop notifications may be silent on WSL; the bell still rings. | Confirm on a real desktop; fall back to `wsl-notify-send` if needed. |
| 7 | Rename is stored as a `hiero.name` label because hc has no rename. | `hc ls` and other tools still show the original name. | Use a real rename if herdr-core adds one. |
| 10 | Pull requests (`P`) are only tested up to the push failing cleanly (no remote here); `gh pr create` itself is untested. | The PR step may need fixes on first real use. | Test against a real GitHub repo. |
| 11 | Merge needs the main working tree on the task's base branch with no uncommitted changes, and stops at conflicts (it undoes the merge and says so). | You resolve conflicts by hand: attach to the agent or work in the worktree, then merge again. | A "rebase onto base" action, and letting the agent resolve conflicts. |
| 12 | Every running Hiero writes timeline notes, so two boards open at once record each note twice. | Duplicate timeline lines. | Let only one Hiero write notes (a lease in the doc). |
| 13 | New files show in review only up to 500 lines; binary files show as "(binary file)". | Large generated files are cut short in the diff view. | Fine for review; open the worktree for the rest. |
| 14 | A pipeline only moves forward while the board that drives it is open. | Close Hiero and the run pauses; reopen and press `A` on the run (the old board id is gone). | Adopt runs automatically when their driver has been silent for a while. |
| 15 | The built-in `plan-build-review` (Claude) is untested end to end to save tokens; only the command/`sh -c` smoke pipeline ran. Agents that never report status (plain shells) never finish a stage. | A real Claude pipeline may need prompt tuning. | Run it on a real task; let shell members finish on exit only. |
| 16 | Agents on other machines show no git branch and can't use worktrees. | Less information for remote agents. | Read git facts through `hc exec --on`. |
| 17 | No release binaries or AUR entry yet: the PKGBUILD builds a local checkout, and its `url` is a placeholder. | Install from source (below). | Push the repository, tag a release, point the PKGBUILD at the tarball. |
| ✅ | Agents on PATH always got the task as the last argument. | — | Phase 6: `[agents.<name>] task = "send"` types it in instead (or `"none"`). |
| ✅ | The board logged to stderr, which drew over the screen. | — | Phase 6: the board logs to `~/.local/state/hiero/hiero.log`. |
| ✅ | Real coding agents showed `·` (no status). | — | Phase 3: Claude Code 2.1.319 reports natively (verified); hooks cover older versions; `hiero report` for others. |
| ✅ | Only tested with shells and fake agents. | — | Phase 3: verified with real Claude Code sessions (working, blocked:permission, done). |
| ✅ | Coming back from full-screen attach quit the board: ratatui's `clear()` asks for the cursor position and the reply was lost. | — | Fixed in Phase 2: clear the screen directly and build a fresh terminal handle. |
| ✅ | The board could attach to its own terminal (endless loop). | — | Fixed in Phase 2: attach, open, kill and preview skip the terminal Hiero runs in. |

## Install

Needs [herdr-core](https://github.com/herdrdev/herdr-core) running (`hc server status`).

```sh
# Arch: build and install a package from this checkout
cd packaging/arch && makepkg -si

# anywhere with Rust
cargo install --path crates/hiero
```

Then run `hiero`. Optional: `hiero config init` writes a commented settings file.

## Configuration

`~/.config/hiero/config.toml` — every setting has a default, so the file can stay short:

```toml
[board]
grouping = "tree"            # project, tree or flat

[notify]
bell = false
on = ["blocked", "error", "pipeline"]

[agents.claude]
command = "claude --model opus"

[agents.pi]
task = "send"                # type the task in after start, instead of passing it as an argument

[keys]
kill = "X"                   # the old key (K) stops working
```

`hiero config show` checks the file and prints what's in effect.

## Getting started

Requires a running herdr-core (`hc server status`) and a Rust toolchain.

```sh
cargo build
cargo test
cargo run -p hiero                          # the board

cargo run -p hiero -- debug status          # handshake and core info
cargo run -p hiero -- debug ls --all        # terminals
cargo run -p hiero -- debug events          # follow live events
cargo run -p hiero -- debug read t1 --lines 50
cargo run -p hiero -- debug doc '{"hello":"world"}'   # write doc "hiero"; omit JSON to read
cargo run -p hiero -- debug type t1 'ls\r'  # type raw bytes over an attach stream

cargo run -p hiero -- integrate claude --status   # is the Claude bridge installed?
cargo run -p hiero -- integrate claude            # install (declines on Claude Code 2.1.319+, which reports natively)
cargo run -p hiero -- integrate codex             # prints the Codex notify line
hiero report blocked --kind question --msg "pick a database"   # from inside an hc terminal
hiero broadcast --label hiero.project=shosha "please rebase on main"
```

| Setting | Meaning |
|---|---|
| `HC_SOCKET` | Use this core socket |
| `HC_SESSION` | Use a separate hc session (default `default`) |
| `HIERO_LOG` | Log level, for example `debug` or `hc_client=trace` |
| `HIERO_NO_NOTIFY` | Turn off desktop notifications (the bell still rings) |
| `VISUAL` / `EDITOR` | Editor for templates and pipelines (default `vi`) |
| `XDG_CONFIG_HOME` | Where `hiero/config.toml` and `hiero/pipelines/` live (default `~/.config`) |
| `XDG_STATE_HOME` | Where `hiero/hiero.log` and `hiero/ui.json` live (default `~/.local/state`) |

### Board keys

| Key | Action |
|---|---|
| `j` `k` / `↑` `↓` | select |
| `g` `G` | first / last |
| `n` / `Tab` | next agent that needs you |
| `x` | dismiss from NEEDS YOU (comes back on its next status change) |
| `/` | fuzzy search; `Esc` clears |
| `p` | grouping: project → tree (children under parents) → flat |
| `f` | show finished terminals on/off |
| `PgUp` `PgDn` / `Ctrl+U` `Ctrl+D` | scroll the preview |
| `r` | refresh the preview |
| `c` | new agent |
| `s` | send a message (`Ctrl+F` in the prompt forces it while the agent is blocked) |
| `a` / `Enter` | attach full-screen; `Ctrl+\` (or `Ctrl+]`) comes back |
| `o` | open the agent inside the board; `Ctrl+\` comes back |
| `i` | interrupt (esc or Ctrl+C, whichever the agent uses) |
| `K` | kill (asks first) |
| `d` | remove a finished terminal |
| `e` | keep on/off (a lost program is replaced by a shell) |
| `R` | rename |
| `t` | templates: `r` run · `e` edit · `n` new · `D` delete |
| `v` | review this agent's task: `j`/`k` file · `PgUp`/`PgDn` scroll · `m` merge · `P` pull request · `K` keep · `D` discard · `r` refresh |
| `T` | task list: `Enter` review · `g` go to agent · `D` forget a finished task |
| `w` | pipelines: `Enter` launch / go to run · `e` edit · `S` stop run · `A` adopt run · `D` forget run |
| `B` | broadcast a message to every live agent shown |
| `u` | Claude token usage per agent |
| `?` | help |
| `q` / `Ctrl+C` | quit |

Status symbols: `●` working · `◐` blocked · `✓` done · `✗` error · `○` idle · `·` no status · `■` finished.
Notifications use `notify-send` when present, otherwise a Windows toast on WSL.

Try it with fake agents that report status (OSC 7501):

```sh
hc run --bg --name worker sh -c "printf '\033]7501;state=working\033\\\\'; sleep 600"
hc run --bg --name asker  sh -c "sleep 3; printf '\033]7501;state=blocked:kind=permission\033\\\\'; sleep 600"
```


## Layout

```
crates/
├── hc-client/src/
│   ├── endpoint.rs   find the socket
│   ├── client.rs     JSON-RPC calls
│   ├── error.rs      typed errors
│   ├── types.rs      Terminal, Status, RunParams, …
│   ├── events.rs     subscribe, notifications, Watcher
│   ├── framed.rs     framed mode: AttachStream
│   └── state.rs      TerminalSet
└── hiero/src/
    ├── main.rs       CLI entry
    ├── tui.rs        event loop: keys, core events, screen reads, timers
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
```

## License

Apache-2.0
