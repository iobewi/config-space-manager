# config-space-manager-esp-storage

ESP storage backend for `config-space-manager`, implemented on top of `esp-storage-manager`.

The repository keeps the architecture split into two layers:

```text
config-space-manager
        │
        └── backends/esp-storage-manager
                  │
                  ▼
          esp-storage-manager
```

The core crate stays hardware-agnostic. This backend owns only the mapping
between ConfigSpace semantics and ESP NVS physical storage.

It does not know about Wi-Fi, TLS, GPIO, agent identity, runtime configuration,
lifecycle state or OTA state.
