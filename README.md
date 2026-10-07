# TeleRoute

TeleRoute is a Windows 10/11 Telegram transport router inspired by the documented Telegram WebSocket transport and TG WS Proxy-style architecture, with a separate UDP/TUN path intended for Telegram realtime media.

## What it does

- Local SOCKS5 frontend at `127.0.0.1:1080` by default.
- Telegram-aware route classification.
- WSS transport using Telegram WebSocket endpoints when the client presents a compatible obfuscated MTProto header.
- A small per-DC pool of pre-established WSS sessions to reduce first-connection handshake latency.
- AES-CTR obfuscated transport state handling.
- TCP fallback when WSS cannot be established.
- Connection counters and technical statistics.
- Windows Wintun L3 adapter for selective Telegram UDP routing.
- Persistent UDP flows rather than one socket per packet.
- Optional QUIC UDP relay for networks that block direct Telegram UDP.
- Diagnostics, rotating logs, tray, autostart and settings.

## Important call-support boundary

A SOCKS5 proxy or MTProto/WSS transport by itself is not enough to guarantee Telegram Voice/Video Calls. TeleRoute therefore uses a separate low-latency UDP path:

```text
Telegram Desktop
  ├─ TCP/MTProto ─> Local SOCKS5 ─> WSS/TCP fallback ─> Telegram
  └─ UDP media  ─> Windows route ─> Wintun ─> UDP userspace flow ─> Telegram
                                            └─ optional QUIC relay
```

The current implementation deliberately does **not** force all Windows traffic through the TUN device. It only routes configured Telegram UDP CIDRs. This preserves ordinary Internet traffic and avoids pretending that a full TCP userspace stack exists.

For the most deterministic non-P2P call path, Telegram should be configured to avoid direct peer-to-peer media so that media remains on Telegram relay addresses covered by the configured Telegram CIDRs. Arbitrary P2P endpoints require adding their IP ranges to `extra_udp_cidrs` or a future endpoint-learning module.

## Modes

- **Proxy** — SOCKS5 + WSS/TCP only.
- **Calls** — SOCKS5 + selective TUN/UDP.
- **Full** — both, with Telegram-aware automatic selection. This is the default.

`route_all_traffic = true` is intentionally rejected today because a safe full-tunnel implementation needs a complete TCP userspace stack or an additional WFP layer. It is not silently enabled.

## Telegram WebSocket transport

The WSS endpoint templates are configuration-driven. The application sends the WebSocket subprotocol `binary` and retains TLS certificate verification.

## Configuration

The generated user configuration lives in the OS per-user application data directory as `config.toml`.

See `config.example.toml` for all fields.

## Testing status in this environment

### Verified by source inspection

- Project layout and transport separation.
- SOCKS5 CONNECT parser, domain/IPv4/IPv6 handling and optional username/password authentication.
- WSS request construction and TLS-enabled WebSocket client.
- Preservation of the 64-byte MTProto obfuscated header before TCP fallback.
- Wintun adapter lifecycle and selective route cleanup logic.
- UDP flow persistence and packet checksum construction.
- Diagnostics and logging paths.

### Partially verified

- Telegram obfuscated transport interoperability: the implementation follows the publicly documented 64-byte transport shape and the corresponding open-source Telegram Desktop logic, but a Windows Telegram Desktop end-to-end run is still required.
- QUIC relay path: protocol and flow mapping are implemented, but a public relay deployment is required for a real WAN test.

### Not testable in the current build container

- Windows Wintun runtime behavior.
- Windows route installation/removal.
- Real Telegram Desktop Voice calls.
- Real Telegram Desktop Video calls.
- Network-change recovery on Windows.
- 24-hour Windows stress stability.

The container does not contain the Rust toolchain or a Windows runtime, so no claim of a successful Windows build or a completed real Telegram call is made here.

## Build

See `BUILD.md`.

```powershell
cargo build --release
```

## Logs

Only technical metadata is logged: connection state, transport, DC, counters, latency and errors. TeleRoute does not intentionally log Telegram message content, audio/video payloads, cookies or credentials.

## Security

- TLS certificate verification remains enabled.
- Wintun is loaded through `wintun-bindings` with signature-verification support enabled.
- The optional relay requires an explicitly configured endpoint and can use an authentication token.
- Privileged Windows changes are limited to adapter/route lifecycle operations.

## License

MIT. See `LICENSE`.

## Quick build

On Windows, install Rust and the MSVC C++ build tools, then double-click `build.bat`.
The executables are created under `target\\release`.

For CI, the repository includes `.github/workflows/build-windows.yml`; run the workflow manually to produce a Windows x64 artifact.

## GUI and Windows launch

The main `tele-route.exe` binary is built as a Windows GUI application, so launching it by double-click does **not** open a console window. Runtime diagnostics and errors are written to the application log directory instead. The `call-relay.exe` helper remains a console/server binary intentionally.

The GUI is responsive: when the window becomes narrow, the left navigation automatically changes to a compact horizontal navigation bar, dashboard cards reflow into fewer columns, and controls size themselves against the available width. The main window can be resized freely within its minimum supported size.

## Standalone Windows build

The Windows release workflow downloads the official signed Wintun 0.14.1 DLL, verifies its SHA-256, embeds it into `TeleRoute.exe`, and publishes a single `TeleRoute.exe` artifact. On first TUN start, the embedded DLL is materialized under the Windows temporary directory automatically; the user does not need to download or place `wintun.dll` manually.
