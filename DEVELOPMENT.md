# DEVELOPMENT

## Architecture

The code is split into a GUI process and an async networking core in the same executable. GUI code never performs blocking network work.

```text
src/
  app.rs                 lifecycle, state and async runtime
  config/                persistent TOML configuration
  proxy/                 SOCKS5 frontend
  telegram/              DC mapping + MTProto obfuscated header handling
  websocket.rs           WSS transport and TCP fallback bridge
  routing/               transport selection
  tun/                   Windows Wintun packet path
  udp/                   UDP flow primitives and relay framing
  calls/                 call path + optional QUIC relay
  diagnostics.rs         health checks
  logging.rs             rotating technical logs
  statistics.rs          atomics and snapshots
  platform/              Windows startup/elevation integration
  gui/                   eframe UI + tray
```

## Phase checklist

1. Architecture + GUI skeleton — source implemented.
2. SOCKS5 — implemented.
3. Telegram DC detection — implemented from the obfuscated 64-byte header and host/IP hints.
4. WSS transport — implemented with TLS verification and `Sec-WebSocket-Protocol: binary`.
5. TCP fallback — implemented with preserved initial obfuscated header.
6. TUN — implemented with Wintun and selective Telegram UDP routes.
7. UDP — implemented with persistent userspace UDP flows and packet injection back into TUN.
8. Call routing — direct UDP first, optional QUIC relay fallback.
9. Diagnostics — implemented.
10. Optimization — bounded buffers/async I/O; further profiling remains a Windows test task.
11. Packaging — documented.

## Why selective TUN instead of a fake full tunnel

Windows routes do not distinguish TCP from UDP. A naive 0/0 TUN route would also capture Telegram TCP and normal applications. TeleRoute therefore installs only configured UDP CIDR routes, leaving regular TCP/direct Internet intact. A true full-tunnel mode would need an additional userspace TCP/IP stack or a WFP layer. The setting is intentionally rejected rather than silently breaking networking.

## Dependency rationale

- `tokio`: async runtime and sockets.
- `tokio-tungstenite`: WSS client with TLS verification.
- `aes` + `ctr`: MTProto obfuscated transport cipher state.
- `quinn`: optional low-latency UDP relay over QUIC.
- `wintun-bindings`: signed Windows L3 tunnel integration.
- `eframe`/`egui`: native Rust GUI.
- `tray-icon`: system tray integration.
- `serde`/`toml`: human-editable configuration.
- `windows-sys` + `winreg`: Windows networking/registry APIs.

No dependency is used to bypass certificate verification.
