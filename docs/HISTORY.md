# Development history

The short table is in the [README](../README.md#development-history). This file keeps the
per-phase checklists as they were written while each phase was built.

## Workspace redesign (W1 to W5)

Hiero became a terminal workspace. A sidebar lists projects, each with named workspaces you
create and sort by hand. A workspace has a folder and shows its terminals in tabs and splits
(nvim, Claude Code, shells). Every pane is an hc terminal, so it keeps running in the background.
The layout is remembered, and each pane stores its command so a missing terminal can be started again.

| Phase | What | State |
|---|---|---|
| W1 | Workspace data model in hc doc `hiero-ws`, plus `hiero ws` CLI | ✅ done |
| W2 | Sidebar and one live pane | ✅ done |
| W3 | Tabs and splits | ✅ done |
| W4 | Restore terminals, and a menu with the old views (board, tasks, pipelines) | ✅ done |
| W5 | Polish | ✅ done |

**W2 ✅** `hiero` now opens the workspace screen: a sidebar of projects and workspaces, the open
workspace's terminal in the middle, and a one-line bar at the bottom. The old agent board is still
there as `hiero board`.

- Every pane is an hc terminal. Opening a workspace attaches to its terminal if it still runs, or
  starts a shell in the workspace folder (labels `hiero.ws`, `hiero.pane`, `hiero.project`) and saves
  its address in the doc. Switching workspaces or quitting Hiero leaves programs running.
- W2 showed only one pane. W3 shows tabs and splits.
- Hiero's own keys sit behind a prefix key (default `ctrl+b`, like tmux), so nvim and Claude get
  every other key. Change it with `[workspace] prefix = "ctrl+a"` (`ctrl+<letter>` or `ctrl+space`).
  `hiero debug keys` shows what your terminal sends, if a prefix doesn't seem to work.
- The sidebar shows a dot per workspace (● working, ◐ needs you, ✓ done) and a dot on a collapsed
  project. The bottom bar names a workspace that needs you. Changes to attention or done ring the
  bell and notify, using the `[notify]` rules.
- `hiero ws …` edits show up live. The open workspace is remembered (`remember = true`).
- When a pane's program ends, the title says so and `Enter` starts a fresh shell there.
- Deleting a workspace or project asks what to do with its terminals (W4).

**W3 ✅** The workspace area has a tabs bar and shows every pane of the active tab at once.

- Tabs bar on top: the tab names in order, the active one highlighted. No status dots here; the
  sidebar has them.
- Each pane has its own border and title (its command or `shell`, and its terminal address, plus
  `ended: …` when its program exited). The focused pane has a thick green border and the cursor.
  Keys and paste go to the focused pane only.
- Every pane keeps its own attach stream. Opening a tab attaches each pane's terminal if it still
  runs, or starts it (its command, in its folder) and saves the address. Switching tabs or
  workspaces closes the old streams, and the programs keep running. Coming back attaches the same
  terminals again.
- Every terminal resize, split, or border move resizes the streams to the pane's inner size.
- `T`, `V`, `S` ask for a command first (for example `nvim .` or `claude`). The command is saved on
  the pane, so a missing terminal can be started again.
- Closing a pane or tab asks first, then kills and removes its terminals.
- Zoom (`z`) is only a view: it is not saved. Moving the focus, splitting, or switching tab ends it.

**W4 ✅** Layouts come back by themselves, and the old features sit behind a menu.

- **Restore.** When a workspace opens, every pane of every tab gets a running terminal, not only
  the visible tab. Missing ones start in the background (the pane's command or a shell, in its
  folder) and the new address is saved. Only the visible tab's panes get attach streams; the other
  tabs are just kept running. Nothing starts twice, and nothing starts before Hiero has seen the
  list of terminals.
- **Restart rule.** A pane's terminal starts again by itself when it is gone from hc, or when it
  finished because the system went down (`server_stop`, `host_shutdown`, `holder_lost`,
  `stopped_by_restart`: a reboot or a core restart). If it ended on its own (`exited`) or you
  killed it, the pane stays as `ended: …` in every tab, and `Enter` starts it again.
- `[workspace] restore = false` turns the automatic start off. Panes without a terminal then say
  "not running", and `Enter` starts one.
- **Delete.** `D` on a workspace or project asks: `y` ends its terminals and deletes, `k` deletes
  and leaves the terminals running, `n` or `Esc` cancels.
- **Menu.** `prefix` then `m` lists Agents board, Tasks, Pipelines, Usage, Templates and
  Broadcast (`j` `k` `Enter`, `Esc` closes). A choice hands the screen to the old board, opened on
  that view. Closing the view (`q`, or `Esc` for the broadcast prompt) brings the workspace back
  as it was: the panes attach to the same terminals again. Programs keep running meanwhile.
- **Worktrees are opt-in.** The switch in the new-agent form (`c` on the board) now starts off.
- The help (`?`) scrolls (`j` `k`) when the terminal is short.

**W5 ✅** Polish.

- **Core restart.** When the core stops and comes back while Hiero is open, the panes come back by
  themselves. A new snapshot after a lost connection checks every pane against the new terminal
  list, using the W4 restart rule and `[workspace] restore`: missing terminals start again (the
  other tabs too), a pane whose stream was lost but whose terminal still runs attaches again, and a
  program you ended stays `ended`. The bar says `core restarted — restored N panes`. Starting waits
  until the request connection works again.
- **Menu views are overlays.** Confirming an action inside Tasks, Pipelines, Usage or Templates
  keeps you in that view; `q` or `Esc` from the view goes back to the workspace. Jumping to an
  agent (`g`) lands on the board, and `q` there goes back. Broadcast with no live agents does not
  open the board: the workspace says so.
- **Sidebar.** Prefix `b` hides or shows it, `<` and `>` change its width (4 columns a step, 16 to
  60). Width and hidden state are saved in `ui.json`.
- Version 0.2.0, workspace-first README, refreshed PKGBUILD.

## Agent board phases (0 to 6)

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

- [x] New-agent form has a **worktree** switch (off by default; turn it on with space): Hiero makes
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
