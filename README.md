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
└── integrations  status bridges per agent (Claude Code first)
```

Hiero keeps no database of its own: terminals live in herdr-core, and Hiero's data lives in
herdr-core's document store. Close Hiero, reopen it, and everything is still there.

## Status

| Phase | What | State |
|---|---|---|
| 0 | Foundation: `hc-client` library | ✅ done |
| 1 | Agent board (MVP) | ✅ done |
| 2 | Drive agents: spawn, actions, attach | ✅ done |
| 3 | Status bridge for Claude Code, codex, pi | ⏳ next |
| 4 | Tasks and git worktrees | ☐ |
| 5 | Orchestration: pipelines, broadcast, machines | ☐ |
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

### Phase 3: Status bridge
**Goal:** real status for agents that don't report it themselves.

herdr-core only knows a status when the program reports it through OSC 7501, and Claude Code
doesn't do that natively.

- [ ] `hiero integrate claude`: installs Claude Code hooks
  - `UserPromptSubmit` → working
  - `Notification` / permission prompt → blocked
  - `Stop` → done
- [ ] Report through OSC 7501 on the pane's tty, falling back to a `hiero.status` label
- [ ] Integrations for codex and pi
- [ ] `hiero integrate --uninstall`

**Done when:** Claude agents show accurate working, blocked and done states on the board.

### Phase 4: Tasks and git worktrees
**Goal:** run parallel agents on one repo without them colliding.

A [git worktree](https://git-scm.com/docs/git-worktree) is a second working folder for the
same repo with its own branch checked out, so each agent gets its own copy of the code but
everything shares one history.

- [ ] Task record saved in doc `hiero`: title, prompt, agent, repo, branch, worktree, status timeline, outcome
- [ ] New task creates `../<repo>.hiero/<task>` on its own branch and starts the agent there
- [ ] Review view: diff stat and file diff of the task branch
- [ ] Merge, open a PR (`gh pr create`), keep or discard (removes the worktree and branch)
- [ ] Turn history from `turn` events

**Done when:** you can start 3 tasks on one repo, review each diff, and merge or discard them.

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
    ├── git.rs        repo root and branch, read from .git
    └── notify.rs     bell and desktop notifications
integrations/         agent hook scripts (Phase 3)
```

## License

Apache-2.0
