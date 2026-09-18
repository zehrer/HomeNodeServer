# HomeNode Server

**HomeNode Server** is the local home automation hub for the [HomeNode](https://github.com/zehrer/HomeNode) ecosystem. It discovers, identifies, organizes, and manages smart devices across your home network, featuring built-in **Matter Operational Fabric** discovery, a multi-admin ecosystem matrix, **Philips Hue (Zigbee)** integration, **BTHome BLE** telemetry, **Govee Local LAN** control, and an interactive **Room & Floor View** with one-click scenes and light grouping.

- **Local & Cloud-Free**: Runs entirely on your own hardware inside your home network. Nothing leaves your home.
- **Automated Discovery & Identification**: Discovers IP devices across Wi-Fi, Ethernet, and Thread; identifies vendors, hardware models, and web UIs using a hot-reloadable Rhai scripting engine and structured hardware catalog.
- **Matter Multi-Admin Native**: Automatically tracks Matter operational nodes across Apple Home, Home Assistant, and vendor fabrics, surfacing co-managed devices in a dedicated cross-ecosystem matrix.
- **Philips Hue & Zigbee**: Discovers Philips Hue Bridges on the LAN, connects via Hue API, monitors Zigbee lights and sensors, and synchronizes room names bi-directionally.
- **BTHome BLE Telemetry**: Receives live sensor broadcasts (temperature, humidity, illuminance, battery, contact, button clicks) from Shelly BLU and BTHome V1/V2 beacons via local Shelly BLE gateways.
- **Govee Local LAN Control**: Discovers and controls Wi-Fi Govee lights and LED strips directly over local UDP multicast without cloud reliance.
- **Room & Floor Structure**: Organizes devices into floors and rooms with customizable emoji icons, room scenes (*Alles An*, *Alles Aus*, *Gemütlich*, *Hell*, *Abend*), and merged light groups (combining multiple lamps into a single luminaire).
- **Network Interfaces**: Full visibility into all device network interfaces (LAN, Wi-Fi, BLE, Zigbee, ARP, mDNS, TR-064) with dual-homed merge capabilities.
- **Modular Runtime**: A robust Rust supervisor that manages device integration modules as isolated child processes over a fast local Unix domain socket (gRPC) control plane.

For the full architectural vision and ecosystem overview, visit the [HomeNode Server Wiki](https://github.com/zehrer/HomeNode/wiki/HomeNode-Server) and the [HomeNode Project Wiki](https://github.com/zehrer/HomeNode/wiki).

---

## Repository layout

- `crates/homenode-server`: Supervisor daemon, process lifecycle management, gRPC control plane, and runtime integration tests
- `crates/homenode-sdk`: Shared gRPC protobuf contract, UDS client helpers, device records, and module environment bindings
- `crates/homenode-definitions`: Rhai device classification engine, hardware catalog database (`catalog.json`), room/floor persistence (`rooms.rs`), light groups (`light_groups.rs`), and ignore lists (`ignore_list.rs`)
- `definitions/`:
  - `devices/*.rhai`: Hot-reloadable Rhai scripts classifying device categories, icons, and capabilities
  - `catalog.json`: Comprehensive hardware catalog (vendors, products, OUI prefixes, default ports, documentation links)
- `modules/network-discovery`: Network discovery daemon (passive ARP, active ICMP sweep, reverse DNS, Web UI detection, and Matter operational node discovery)
- `modules/philips-hue`: Philips Hue Bridge integration (link-button pairing, Zigbee lights & sensors sync, real-time control, bi-directional room sync)
- `modules/bthome`: BTHome BLE receiver & decoder for Shelly BLU and BTHome V1/V2 sensors via local Shelly gateway webhooks
- `modules/web`: Interactive Web HMI dashboard:
  - **Dashboard (`/`)**: Real-time energy flow, live BLE events, and quick jump navigation
  - **Devices (`/devices`)**: Complete device inventory, network interface filters, inspector drawer, dual-homed linking, documentation notes
  - **Rooms (`/rooms`)**: Multi-floor / room navigation, hero lighting controls, merged light luminaires, one-click lighting scenes, live sensor telemetry strip, and room manager modal
  - **Govee (`/govee`)**: Local UDP discovery and direct LAN light control (power, brightness, RGB color)
  - **Matter (`/matter`)**: Matter operational fabrics, Multi-Admin matrix (Apple Home, Home Assistant)
- `modules/matter-controller`: Matter controller foundation
- `modules/matter-bridge`: Matter bridge foundation
- `archive/rs-matterd`: Archived legacy `rs-matterd` prototype and packaging scripts for reference

---

## Architecture

- The supervisor owns the Unix domain socket gRPC endpoint (`/tmp/homenode.sock`).
- Modules run as isolated child processes and receive configuration via environment variables plus a TOML config path.
- Modules register with the supervisor, report health, and upsert device records to the central inventory.
- The web module reads aggregated runtime snapshots from the supervisor and serves the web interface.

See [docs/architecture.md](docs/architecture.md) for the architecture specification and module details.

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

Once started, open your browser at **[http://localhost:8080](http://localhost:8080)**:

- **🏠 Dashboard**: [http://localhost:8080](http://localhost:8080) — real-time overview, live sensor events, Fronius solar / Shelly 3-phase energy.
- **🚪 Room View**: [http://localhost:8080/rooms](http://localhost:8080/rooms) — floor and room navigation, lighting controls, scenes, merged lamps, sensor telemetry.
- **📱 Devices View**: [http://localhost:8080/devices](http://localhost:8080/devices) — browse detected devices, filter by network interfaces (ARP, Ping, Hue, BTHome, Matter, TR-064), inspect specs, link interfaces.
- **💡 Govee LAN Control**: [http://localhost:8080/govee](http://localhost:8080/govee) — local Wi-Fi control for Govee lights without cloud connection.
- **✨ Matter Fabrics**: [http://localhost:8080/matter](http://localhost:8080/matter) — view commissioned Matter nodes, Apple Home & Home Assistant fabrics, Multi-Admin matrix.

*(HTTP port and options can be configured in `config/modules/web.example.toml` or your local `web.toml`).*

---

## Running Tests

Run the full workspace unit, catalog, script, and supervisor integration test suite:

```sh
cargo test --workspace
```

---

## Archived rs-matterd

The previous `rs-matterd` prototype is preserved under [`archive/rs-matterd`](archive/rs-matterd). Its packaging scripts and documentation remain available for reference.
