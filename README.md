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
| 5 | Orchestration: pipelines, broadcast, machines | ⏳ next |
| 6 | Polish: usage, config, packaging | ☐ |

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

### Phase 5: Orchestration
**Goal:** agents working together.

- [ ] Pipelines in TOML, for example `plan → [worker × N] → review`, built on parent/child terminals
- [ ] Broadcast a message to every agent matching a label filter
- [ ] Tree view of parents and their children
- [ ] Remote machines: pick a machine when spawning; remote terminals on the same board

### Phase 6: Polish
- [ ] Usage and cost panel from agent transcripts
- [ ] `~/.config/hiero/config.toml`: keybindings, theme, notification rules
- [ ] Restore layout and selection, help overlay, log file
- [ ] AUR `PKGBUILD` and release binaries

## Known issues

Tracked here until fixed. ✅ = fixed.

| # | Issue | Impact | Plan |
|---|---|---|---|
| 8 | Before its first report an agent shows `·`, including while Claude waits at its "trust this folder?" dialog, which comes before any hook or native report. | A new agent in a new folder looks idle while it actually waits for you. | Show "starting / not reported" for young agents, and hint to attach. |
| 9 | Codex (`hiero hook codex`) and pi are untested; neither is installed here. Codex `notify` only reports finished turns, so Codex shows done but never working or blocked. | Partial status for Codex; none for pi unless its hooks call `hiero report`. | Test when installed; check whether they speak OSC 7501 natively. |
| 3 | Opening an agent inside the board (`o`) resizes the agent to the pane. herdr-core gives the size to the last active viewer. | The agent reflows to a smaller size; full-screen attach (`a`) sizes it back. | Could use an observe-only stream for viewing and become a viewer only on the first keypress. |
| 4 | The board redraws timers and the spinner every second, so its screen never goes quiet. | When Hiero itself runs inside an hc terminal, `hc send` / `hc keys` into it fail with "output has not settled". Normal use is unaffected. | Redraw only when something visible changes, and tick wait timers less often. |
| 5 | For agents on PATH the task goes in as the last argument (`claude "<task>"`). | Agents that take their prompt differently start without the task. | Use a template for those; later, per-agent launch settings in Hiero's config. |
| 6 | The Windows toast (WSL) is sent with exit code 0, but nobody has confirmed it actually appears. | Desktop notifications may be silent on WSL; the bell still rings. | Confirm on a real desktop; fall back to `wsl-notify-send` if needed. |
| 7 | Rename is stored as a `hiero.name` label because hc has no rename. | `hc ls` and other tools still show the original name. | Use a real rename if herdr-core adds one. |
| 10 | Pull requests (`P`) are only tested up to the push failing cleanly (no remote here); `gh pr create` itself is untested. | The PR step may need fixes on first real use. | Test against a real GitHub repo. |
| 11 | Merge needs the main working tree on the task's base branch with no uncommitted changes, and stops at conflicts (it undoes the merge and says so). | You resolve conflicts by hand: attach to the agent or work in the worktree, then merge again. | A "rebase onto base" action, and letting the agent resolve conflicts. |
| 12 | Every running Hiero writes timeline notes, so two boards open at once record each note twice. | Duplicate timeline lines. | Let only one Hiero write notes (a lease in the doc). |
| 13 | New files show in review only up to 500 lines; binary files show as "(binary file)". | Large generated files are cut short in the diff view. | Fine for review; open the worktree for the rest. |
| ✅ | Real coding agents showed `·` (no status). | — | Phase 3: Claude Code 2.1.319 reports natively (verified); hooks cover older versions; `hiero report` for others. |
| ✅ | Only tested with shells and fake agents. | — | Phase 3: verified with real Claude Code sessions (working, blocked:permission, done). |
| ✅ | Coming back from full-screen attach quit the board: ratatui's `clear()` asks for the cursor position and the reply was lost. | — | Fixed in Phase 2: clear the screen directly and build a fresh terminal handle. |
| ✅ | The board could attach to its own terminal (endless loop). | — | Fixed in Phase 2: attach, open, kill and preview skip the terminal Hiero runs in. |

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
```

| Setting | Meaning |
|---|---|
| `HC_SOCKET` | Use this core socket |
| `HC_SESSION` | Use a separate hc session (default `default`) |
| `HIERO_LOG` | Log level, for example `debug` or `hc_client=trace` |
| `HIERO_NO_NOTIFY` | Turn off desktop notifications (the bell still rings) |
| `VISUAL` / `EDITOR` | Editor for templates (default `vi`) |

### Board keys

| Key | Action |
|---|---|
| `j` `k` / `↑` `↓` | select |
| `g` `G` | first / last |
| `n` / `Tab` | next agent that needs you |
| `x` | dismiss from NEEDS YOU (comes back on its next status change) |
| `/` | fuzzy search; `Esc` clears |
| `p` | group by project on/off |
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
    ├── git.rs        repo root and branch, read from .git
    └── notify.rs     bell and desktop notifications
```

## License

Apache-2.0
