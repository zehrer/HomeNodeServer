# HomeNode Server

**HomeNode Server** is the local home automation hub for the [HomeNode](https://github.com/zehrer/HomeNode) ecosystem. It discovers, identifies, organizes, and manages smart devices across your home network, featuring built-in **Matter Operational Fabric** discovery, a multi-admin ecosystem matrix, and foundations for **Matter bridge** and **Matter controller** capabilities.

- **Local & Cloud-Free**: Runs entirely on your own hardware inside your home network. Nothing leaves your home.
- **Automated Discovery & Identification**: Discovers IP devices across Wi-Fi, Ethernet, and Thread; identifies vendors, hardware models, and web UIs using a hot-reloadable Rhai scripting engine and structured hardware catalog.
- **Matter Multi-Admin Native**: Automatically tracks Matter operational nodes across Apple Home, Home Assistant, and vendor fabrics, surfacing co-managed devices in a dedicated cross-ecosystem matrix.
- **Modular Runtime**: A robust Rust supervisor that manages device integration modules as isolated child processes over a fast local Unix domain socket (gRPC) control plane.
- **Protocol Independence (Current & Future)**:
  - **Currently Supported**: IP-based devices (LAN, Wi-Fi), Thread border router announcements, mDNS/DNS-SD services, ARP, Web HMIs, and Matter operational fabrics.
  - **Planned for Later Options**: Direct Zigbee (via USB/network coordinator), MQTT brokers, BLE local mesh, and dedicated vendor integrations (Shelly, SwitchBot, Govee).

For the full architectural vision and ecosystem overview, visit the [HomeNode Server Wiki](https://github.com/zehrer/HomeNode/wiki/HomeNode-Server) and the [HomeNode Project Wiki](https://github.com/zehrer/HomeNode/wiki).

---

## Repository layout

- `crates/homenode-server`: Supervisor daemon, process lifecycle management, gRPC control plane, and runtime integration tests
- `crates/homenode-sdk`: Shared gRPC protobuf contract, UDS client helpers, device records, and module environment bindings
- `crates/homenode-definitions`: Rhai device classification engine, hardware catalog database (`catalog.json`), and vendor/product matching
- `definitions/`:
  - `devices/*.rhai`: Hot-reloadable Rhai scripts classifying device categories, icons, and capabilities
  - `catalog.json`: Comprehensive hardware catalog (vendors, products, OUI prefixes, default ports, documentation links)
- `modules/network-discovery`: Network discovery daemon (passive ARP, active ICMP sweep, reverse DNS, Web UI detection, and Matter operational node discovery)
- `modules/web`: Interactive Web HMI dashboard (category filtering, device inspector, custom category overrides, product assignments, persistent documentation notes) and dedicated **Matter Fabrics HMI** (`/matter`)
- `modules/matter-controller`: Matter controller foundation
- `modules/matter-bridge`: Matter bridge foundation
- `archive/rs-matterd`: Archived legacy `rs-matterd` prototype and packaging scripts for reference

---

## Architecture

- The supervisor owns the Unix domain socket gRPC endpoint (`/tmp/homenode.sock`).
- Modules run as isolated child processes and receive configuration via environment variables plus a TOML config path.
- Modules register with the supervisor, report health, and upsert device records to the central inventory.
- The web module reads aggregated runtime snapshots from the supervisor and serves the web interface.

See [docs/architecture.md](docs/architecture.md) for the architecture specification and reserved module IDs.

---

## Quick start

### 1. Build the workspace

```sh
cargo build --workspace
```

> **Tip for macOS development**: If using Apple Command Line Tools where the linker complains about unknown architectures in `.tbd` files, ensure a stable macOS SDK is selected:
> ```sh
> export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk
> ```

### 2. Run HomeNode Server

You can run the supervisor directly without any arguments:

```sh
cargo run -p homenode-server
```

When started without `--config`, HomeNode Server automatically looks for `config/server.toml` (your local custom configuration), and gracefully falls back to `config/server.example.toml` if no custom file exists.

To explicitly point to a custom configuration file:

```sh
cargo run -p homenode-server -- --config path/to/your-server.toml
```

### 3. Open the Web Dashboard

Once started, open your browser:

- **Main Dashboard**: [http://localhost:8080](http://localhost:8080) — browse detected devices, filter by 20 categories, inspect hardware specs, override categories, and open Web UIs.
- **Matter Fabrics Dashboard**: [http://localhost:8080/matter](http://localhost:8080/matter) — view commissioned Matter nodes, Apple Home & Home Assistant fabrics, Multi-Admin coverage, and cross-ecosystem matrix.

*(HTTP port and title can be customized in `config/modules/web.example.toml` or your local `web.toml`).*

---

## Running Tests

Run the full workspace unit, catalog, script, and supervisor integration test suite:

```sh
cargo test --workspace
```

---

## Archived rs-matterd

The previous `rs-matterd` prototype is preserved under [`archive/rs-matterd`](archive/rs-matterd). Its packaging scripts and documentation remain available for reference.
