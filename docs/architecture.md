# HomeNode Server Architecture

## Phase 1 shape

`homenode-server` is the only long-running core daemon. It provides a local
control plane over gRPC on a Unix domain socket and starts enabled modules as
child processes.

The current module classes are:

- `web`: HTTP status surface backed by runtime snapshots
- `matter-controller`: placeholder for future controller features
- `matter-bridge`: placeholder for future bridge features
- `network-discovery`: placeholder for network inventory and standard discovery

Reserved module IDs for later phases:

- `shelly`
- `govee`
- `zigbee`
- `switchbot`
- `native-devices`
- `ai-local`
- `extensions`

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
