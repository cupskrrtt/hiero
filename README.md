# hiero

A TUI agent manager on top of [herdr-core](https://github.com/herdrdev/herdr-core) (`hc`).

- `crates/hc-client`: typed async client for the hc JSON-RPC API (calls, events, auto-reconnecting watcher, terminal state).
- `crates/hiero`: the app. The board UI arrives in Phase 1; for now there are `hiero debug` commands.

```sh
cargo run -p hiero -- debug status
cargo run -p hiero -- debug ls --all
cargo run -p hiero -- debug events
cargo run -p hiero -- debug read t1 --lines 50
cargo run -p hiero -- debug doc '{"hello":"world"}'
```

Socket: `HC_SOCKET`, else `$XDG_RUNTIME_DIR/herdr-core/<HC_SESSION|default>.sock`.
Logs: `HIERO_LOG=debug`.
