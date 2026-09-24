# config-space-manager

`config-space-manager` is a small `no_std` configuration-storage ownership layer for embedded systems.

A component claims an isolated opaque space:

```rust
let wifi = manager.claim("wifi", Budget::new(512))?;
```

If the backend can guarantee the requested reservation, the component receives a `ConfigSpace` capability. It owns the byte encoding inside that space:

```rust
let current = wifi.load().await?;
wifi.commit(serialized_wifi_config).await?;
```

The manager does **not** know what an SSID, certificate, token, GPIO, or controller URL is.

## Repository layout

```text
config-space-manager/
├── src/                    core abstraction, hardware-agnostic
└── backends/
    └── esp-nvs/            ESP/NVS backend using esp-storage-manager
```

The core crate deliberately has no ESP dependency. Hardware-specific persistence
lives in backend crates beside it, not in application repositories.

## Boundary

The core owns:

- unique space ownership;
- reservation/admission control;
- per-space maximum payloads;
- opaque replacement commits;
- generations;
- isolation through capability handles.

Backend crates own:

- physical storage accounting;
- persistence mechanics;
- backend-specific atomic replacement semantics.

Components own:

- schemas and migrations;
- Wi-Fi/TLS/application policy;
- serialization formats;
- provisioning protocols.

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

Each component gets one opaque value, not a nested key/value database. That keeps
the manager schema-agnostic and lets the persistence backend provide atomic
whole-configuration replacement.

## Status

Initial API under active development. The first hardware backend is
`config-space-manager-esp-nvs`, integrated with `esp-storage-manager`.
