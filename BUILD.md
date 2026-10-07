# BUILD

## Requirements

- Windows 10/11 x64 (Windows 11 recommended for development).
- Rust 1.95+ with the MSVC toolchain: `rustup default stable-x86_64-pc-windows-msvc`.
- Visual Studio Build Tools or Visual Studio with **Desktop development with C++** and the Windows SDK.
- Official signed `wintun.dll` placed at `wintun\\wintun.dll`.

The application creates a Wintun Layer-3 adapter, so the first TUN/Calls connection may require administrator elevation. `wintun-bindings` can verify the DLL signature before loading it.

## Build

```powershell
cargo build --release
```

## Run

```powershell
.\target\release\tele-route.exe
```

For first-time Calls mode, run the executable as Administrator so the adapter can be created. After the adapter exists, normal use may require fewer privileged operations depending on Windows policy.

## Call relay

The optional relay is a standalone binary:

```powershell
cargo build --release --bin call-relay
.\target\release\call-relay.exe --bind 0.0.0.0:4433 --cert-der cert.der --key-der key.der --token "a-long-random-token"
```

The client trusts normal system/public CA roots. Do not use a self-signed certificate in production unless the trust configuration is deliberately extended.

## Release layout

```text
release/
  TeleRoute.exe
  config/
    config.example.toml
  wintun/
    wintun.dll
  README.md
  BUILD.md
  DEVELOPMENT.md
  LICENSE
```

`wintun.dll` should come from the official Wintun distribution; do not replace it with an arbitrary DLL.

### Standalone Windows build

The Windows release workflow embeds the official Wintun DLL into the executable. The generated Rust source uses `env!("OUT_DIR")` rather than an absolute Windows path, so paths containing backslashes are handled safely.
