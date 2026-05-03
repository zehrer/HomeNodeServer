# HomeNode Server

HomeNode Server is the new modular runtime for HomeNode. The repository now
focuses on a Rust supervisor that starts integration modules as separate
processes and aggregates their status over a local gRPC control plane.

## Repository layout

- `crates/homenode-server`: supervisor daemon and runtime tests
- `crates/homenode-sdk`: shared gRPC contract, UDS client helpers, common types
- `modules/web`: HTTP status module
- `modules/matter-controller`: Matter controller stub
- `modules/matter-bridge`: Matter bridge stub
- `modules/network-discovery`: network discovery stub
- `archive/rs-matterd`: archived legacy `rs-matterd` prototype and packaging

## Architecture

- The supervisor owns the Unix domain socket gRPC endpoint.
- Modules are started as child processes and receive configuration via
  environment variables plus a TOML config path.
- Modules register with the supervisor, report health, and upsert devices.
- The web module reads the aggregated runtime snapshot from the supervisor and
  renders a status page.

See [docs/architecture.md](/Users/stephan/HomeNodeDev/HomeNodeServer/docs/architecture.md)
for the current target architecture and reserved module IDs.

## Quick start

Build the workspace:

```sh
cargo build --workspace
```

Run the supervisor with the example configuration:

```sh
cargo run -p homenode-server -- --config config/server.example.toml
```

The example configuration starts the stub modules through `cargo run`, so it is
usable directly in a development checkout on macOS.

Open the status page from `config/modules/web.example.toml` after startup.

## Archived rs-matterd

The previous `rs-matterd` effort is preserved under
[`archive/rs-matterd`](/Users/stephan/HomeNodeDev/HomeNodeServer/archive/rs-matterd).
Its packaging scripts and documentation remain available for reference, but they
are no longer the active product direction for this repository.
