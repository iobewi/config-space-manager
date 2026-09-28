# config-space-manager

> **Moved to IOBEWI.** The maintained portable core is now
> [`iobewi-config-space`](https://github.com/iobewi/iobewi/tree/main/services/config-space).
> The ESP NVS implementation is
> [`iobewi-esp-config-space`](https://github.com/iobewi/iobewi-esp/tree/main/adapters/config-space).
> This repository remains available for history. New consumers should use the
> `iobewi-config-space` Cargo package and `iobewi_config_space` Rust imports.

`config-space-manager` is a small `no_std`, hardware-agnostic configuration
ownership and quota layer for embedded systems.

A component claims an isolated opaque space:

```rust
let wifi = manager.claim("wifi", Budget::new(512))?;
```

If the backend can guarantee the requested reservation, the component receives
a `ConfigSpace` capability. It owns the byte encoding inside that space:

```rust
let current = wifi.load().await?;
wifi.commit(serialized_wifi_config).await?;
```

The manager does **not** know what an SSID, certificate, token, GPIO,
filesystem, flash sector or NVS namespace is.

## Boundary

The core owns:

- unique space ownership;
- reservation/admission control;
- per-space maximum payloads;
- opaque replacement commits;
- generations;
- isolation through capability handles;
- the generic `ConfigBackend` persistence contract.

Backends own:

- physical persistence;
- storage-specific capacity accounting;
- atomic replacement guarantees;
- hardware/platform health checks where applicable.

Components own:

- schemas and migrations;
- Wi-Fi/TLS/application policy;
- serialization formats;
- provisioning protocols.

Platform-specific adapters intentionally live outside this repository. For ESP,
the `ConfigBackend` implementation is provided by ESPBeWi.

## Capacity model

A logical payload byte is not assumed to equal one physical storage byte.

```text
Budget(max payload)
        ↓
reservation_units()
        ↓
backend-specific capacity accounting
```

A successful claim is a boot-lifetime guarantee: later components cannot consume
capacity already reserved for it.

## Storage model

Each component gets one opaque value, not a nested key/value database. That
keeps the manager schema-agnostic and lets each persistence backend provide
atomic whole-configuration replacement.

## Status

Initial API under active development.
