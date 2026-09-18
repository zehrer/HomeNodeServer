# HomeNode Server Architecture

## Phase 1 shape

`homenode-server` is the only long-running core daemon. It provides a local
control plane over gRPC on a Unix domain socket and starts enabled modules as
child processes.

The current module classes are:

- `web`: HTTP UI & HMI (Dashboard, Devices inventory, Room & Floor view, Govee LAN control, Matter matrix)
- `network-discovery`: Network inventory scanner (ARP, ICMP ping sweep, mDNS, SSDP, TR-064, HTTP probe, Matter discovery)
- `philips-hue`: Local Philips Hue Bridge integration (Zigbee lights & sensors, link-button pairing, real-time control, bi-directional room sync)
- `bthome`: Passive BLE advertisement receiver and decoder for BTHome V1/V2 sensors (Shelly BLU, beacons) via local Shelly BLE gateways
- `matter-controller`: Foundation for Matter controller features
- `matter-bridge`: Foundation for Matter bridge features

Reserved / future module IDs:

- `tuya`
- `shelly`
- `govee` (Local UDP LAN control currently integrated in `modules/web`)
- `zigbee` (Standalone Zigbee coordinator beyond Hue Bridge)
- `switchbot`
- `native-devices`
- `ai-local`
- `extensions`

## Persistent State Stores

HomeNode Server persists user customizations and state under `data/`:

- `data/docs_store.json`: Device custom names, assigned room IDs, and documentation notes
- `data/rooms_store.json`: Floor & room hierarchy with custom emoji icons
- `data/light_groups_store.json`: Virtual merged light luminaires
- `data/ignore_list.json`: Filtered/ignored devices and noisy BLE MACs

## Runtime contract

The supervisor exposes these RPCs:

- `RegisterModule`
- `ReportHealth`
- `UpsertDevices`
- `GetRuntimeSnapshot`

Every module process receives:

- `HOMENODE_SOCKET_PATH`
- `HOMENODE_MODULE_CONFIG`
- `HOMENODE_MODULE_ID`
- `HOMENODE_SERVER_CONFIG`

Modules use the shared SDK crate to connect over Unix domain sockets and never
link against supervisor-internal Rust types.

## Configuration model

The root TOML file defines server settings and per-module launch entries. Each
module entry has an `enabled` flag, a `module_id`, an executable name, and a
path to a module-specific TOML file. Reserved module IDs can stay disabled with
stub configuration until their implementation exists.
