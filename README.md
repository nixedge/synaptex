# Synaptex

**Self-hosted smart home controller and zero-trust network manager.**

Synaptex runs on your own hardware, speaks directly to IoT devices over the local network (no cloud required for local control), and manages the network layer — firewall rules, DHCP reservations, and device discovery — through a dedicated router daemon. All components are written in strict, async Rust.

---

## Features

- **Local Tuya control** — Speaks Tuya protocol v3.3, v3.4, and v3.5 (AES-128-ECB / CBC / GCM) directly to devices on the LAN; no intermediary cloud hop needed for day-to-day operation.
- **Tuya Cloud integration** — Optional cloud channel for device import, pairing, and firmware queries via HMAC-SHA256-signed API calls.
- **REST API** — Axum HTTP server (port 8080) exposing devices, rooms, groups, routines, and an SSE event stream; secured by a bearer-token API key.
- **Automation routines** — Cron-scheduled and on-demand routines with per-step delays and multi-device targets.
- **Router daemon with gRPC IPC** — A privileged `synaptex-router` process manages nftables firewall rules, Kea DHCP static reservations, and UDP device discovery. It exposes a mTLS-secured gRPC service (port 50052) that `synaptex-core` talks to.
- **Tuya Smart Config (EZ mode)** — Android companion app broadcasts Wi-Fi credentials to unpaired devices via UDP.
- **Embedded persistence** — `sled` embedded database with `dashmap` hot cache; no external database to manage.
- **Structured observability** — `tracing` + `tracing-subscriber` with `RUST_LOG`-based filtering throughout.
- **Nix-reproducible builds** — `flake.nix` with `flake-parts` + `crane` + `fenix` for fully reproducible dev shells and packages.

---

## Tech Stack

| Layer | Technology |
|---|---|
| Language | Rust 2021, async throughout |
| Async runtime | Tokio |
| REST API | Axum 0.7 + Tower HTTP |
| gRPC IPC (router ↔ core) | Tonic 0.12 + Prost, mTLS via `rcgen` |
| HTTP client | Reqwest 0.12 (rustls) |
| Storage | sled 0.34 (embedded), dashmap 6 |
| Serialization | serde + serde_json (REST/wire), postcard (sled) |
| Crypto | aes, aes-gcm, hmac, sha2, crc32fast |
| CLI | Clap 4 |
| Android app | Dioxus 0.6 (mobile) |
| Build | Nix flake-parts + crane + fenix |

---

## Prerequisites

### Rust (cargo)

Install the stable Rust toolchain via [rustup](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

The `synaptex-router-proto` crate generates gRPC bindings at build time and requires `protoc`:

```sh
# Debian/Ubuntu
sudo apt install protobuf-compiler

# macOS
brew install protobuf
```

### Nix (recommended)

[Install Nix](https://nixos.org/download) with flakes enabled. The dev shell provides the correct Rust toolchain, `protoc`, and all system dependencies automatically — no manual setup needed.

---

## Getting Started

### With Nix (recommended)

```sh
git clone https://github.com/nixedge/synaptex
cd synaptex

# Enter the reproducible dev shell
nix develop

# Build all workspace crates
cargo build --workspace

# Run the core daemon (HTTP API on :8080)
cargo run -p synaptex-core

# Run the router daemon (gRPC on :50052)
cargo run -p synaptex-router

# Use the CLI
cargo run -p synaptex-cli -- --help
```

### With cargo only

```sh
git clone https://github.com/nixedge/synaptex
cd synaptex
cargo build --workspace
```

### Configuration

Both daemons are configured via CLI flags or environment variables. Common variables:

| Variable | Default | Description |
|---|---|---|
| `SYNAPTEX_HTTP_PORT` | `8080` | Core REST API port |
| `SYNAPTEX_HTTP_URL` | `http://localhost:8080` | Used by the CLI |
| `SYNAPTEX_API_KEY` | _(none)_ | Bearer token; omit for open mode |
| `SYNAPTEX_ROUTER_LISTEN` | `[::]:50052` | Router gRPC bind address |
| `SYNAPTEX_ROUTER_INTERFACES` | _(all)_ | Comma-separated interfaces for UDP discovery |
| `SYNAPTEX_ROUTER_KEA_SOCKET` | _(none)_ | Path to Kea hook domain socket |
| `RUST_LOG` | `info` | Log filter (e.g. `synaptex_core=debug`) |

A `.envrc` / `.env` file is supported via `direnv` for local development.

---

## Project Structure

```
synaptex/
├── crates/
│   ├── synaptex-types/        # Shared types: DeviceId, DeviceInfo, Capability,
│   │                          #   DevicePlugin trait, DeviceState, bus types
│   ├── synaptex-core/         # Main daemon: sled + dashmap + plugin registry
│   │                          #   + Axum REST server + Tuya Cloud client
│   ├── synaptex-tuya/         # Tuya local TCP plugin: AES-128-ECB/CBC/GCM,
│   │                          #   protocol v3.3/v3.4/v1.5, DP mapping
│   ├── synaptex-cli/          # CLI REST client — all commands via reqwest
│   ├── synaptex-app/          # Dioxus Android app: Smart Config pairing wizard
│   ├── synaptex-router-proto/ # Protobuf definitions + generated gRPC stubs
│   │                          #   for the router <-> core IPC channel
│   └── synaptex-router/       # Router daemon: nftables firewall, Kea DHCP,
│                              #   UDP device discovery, mTLS gRPC server
├── flake.nix                  # Nix flake (flake-parts + crane + fenix)
└── Cargo.toml                 # Workspace manifest
```

### Crate responsibilities

**`synaptex-core`** is the central daemon. It loads device plugins, serves the REST API, maintains the device registry in sled, evaluates automation routines, and communicates with `synaptex-router` over gRPC for network-layer operations.

**`synaptex-router`** runs as a privileged companion process. It owns raw network access: listening for Tuya UDP discovery broadcasts, pushing DHCP static reservations to a Kea control socket, and applying firewall rules via nftables. It exposes a single mTLS-secured gRPC service that `synaptex-core` calls.

**`synaptex-router-proto`** contains the `.proto` schema and the tonic-generated server/client stubs shared by both daemons.

**`synaptex-tuya`** implements the Tuya local protocol (v3.3 / v3.4 / v3.5) as a `DevicePlugin`. It handles the cryptographic handshake, session key derivation, DP encoding, and a reconnect supervisor.

**`synaptex-types`** defines the plugin trait, device capability model, and state bus types shared across crates.

**`synaptex-cli`** is a fully async, reqwest-based CLI for interacting with the `synaptex-core` REST API. It supports device management, room/group control, routine management, Tuya Cloud import, and SSE event watching.

**`synaptex-app`** is a Dioxus 0.6 mobile app targeting Android. It implements Tuya EZ-mode (Smart Config) Wi-Fi provisioning for pairing new devices.

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
