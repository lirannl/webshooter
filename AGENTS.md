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