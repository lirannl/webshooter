# Agent instructions

Hard rules for any agent (or human) modifying this codebase. These are
architectural invariants, not suggestions.

## Network topology: exactly ONE port, shared by HTTPS and WebTransport

The server exposes a single network port, `config.port`:

- The Poem HTTPS listener binds **TCP** `config.port` (`server/src/main.rs`,
  `TcpListener::bind(...config.port...)`).
- The WebTransport/QUIC media endpoint binds **UDP** `config.port`
  (`server/src/wt.rs`, `.with_bind_default(config.port)`).

TCP and UDP are different protocol stacks; the OS allows both to bind the
same port number simultaneously. This is intentional and required: the
browser client opens `new WebTransport(origin)` against the same host
**and port** as the HTTPS origin it downloaded the page from. Any second
port breaks every existing client.

### MUST NOT

- Add any extra port to `Config` (`wt_port`, `quic_port`, `media_port`,
  `udp_port`, etc.) — with or without a serde default.
- Bind either listener to anything other than `config.port`.
- Introduce port-offset logic, CLI port flags, or per-transport ports to
  "resolve a perceived conflict" between the HTTP and QUIC listeners. There
  is no conflict: TCP and UDP share one port number by design.

### MUST

- Keep `server/src/config.rs`, `server/src/wt.rs` and `AGENTS.md` itself in
  sync with the single-port design.

If you believe a second port is genuinely required for some feature, stop
and reconsider the design first — the transport layer is built around port
sharing.

## Where the frontend a client receives comes from

Two different mechanisms, decided by build profile. `rust-embed` is pulled in
without the `debug-embed` feature (`Cargo.toml:42`), so its files are embedded
at **compile** time in release and read from **disk at runtime** in debug. On
top of that, `server/src/frontend.rs` prefers Vite in debug.

### Debug builds

`asset_bytes` tries, in order:

1. `dev_fetch` — Vite on `localhost:5173`. `#[cfg(debug_assertions)]`, and
   `#[cfg_attr(debug_assertions, allow_missing = true)]` sits on the `RustEmbed`
   derive so a missing `dist/` is tolerated.
2. `Assets::get` — and because `debug-embed` is off, this reads `../dist` off
   the filesystem **now**, not from the binary.

So a debug server serves either the Vite dev server or the static files on
disk. It never serves a compile-time bundle, so **rebuilding the server changes
nothing about what a browser receives.**

### Release builds

`dev_fetch` does not exist, and `Assets::get` serves the bundle captured when
the binary was compiled. Release therefore serves the embedded assets
**always**, and `pnpm -C webui build` must be followed by a server build for a
`webui/` change to reach a client.

### MUST NOT

- Rebuild or re-link the server expecting a `webui/` change to reach a browser
  in debug. `touch server/src/frontend.rs && cargo build -p webshooter` is cargo
  cult there: a full server rebuild for no change in the bytes the client gets.
- Diagnose `unparsable server message, ignored` in a client's log as a server
  bug. It means the two ends disagree about the wire format, which in
  development is almost always a **stale client build** — check whether the wasm
  being served predates the protocol change.
- Run `git stash`, `git checkout`, a branch switch, or any other transient
  revert of `shared/` or `webui/` while the Vite dev server is running. Its wasm
  watcher fires on mtime, so it rebuilds `webui/wasm/pkg` from whatever source
  state it happens to observe mid-operation — and the result is *newer* than
  every source file, so a timestamp check reports it as fresh. Rebuild the
  webui after any such operation before trusting what a client receives.
- Enable rust-embed's `debug-embed` feature to "fix" a stale-asset problem. That
  silently moves debug onto the compile-time bundle and trades the confusing
  failure for a subtler one.

### MUST

- Run `pnpm -C webui build` after any change under `webui/`. It is the only step
  that is always required: it updates `webui/wasm/pkg` for the dev server and
  `dist/` for the release bundle.
- Verify changes to `shared/` against **both** crates. `shared/` is compiled
  into the server *and* the wasm client, and `cargo build -p webshooter` does
  not build `webshooter-wasm`. Adding a variant to `ClientDatagram` or
  `ServerDatagram` breaks the client's exhaustive `match` in
  `webui/wasm/src/lib.rs`, which only surfaces during `pnpm -C webui build` —
  so a green server build says nothing about the client compiling.

