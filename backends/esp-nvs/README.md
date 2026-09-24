# config-space-manager-esp-nvs

ESP NVS backend for `config-space-manager`.

This crate is self-contained: it owns the physical `FlashStorage` instance,
the cached ESP NVS view, capacity accounting and ConfigSpace record framing.
It has no dependency on the former `esp-storage-manager` repository.

```text
ConfigManager / ConfigSpace
            │
            ▼
config-space-manager-esp-nvs
            │
            ▼
      ESP NVS / flash
```

Application code cannot address NVS namespaces or keys directly. Persistent
application state is represented as component-owned ConfigSpace objects.

OTA transaction metadata is expected to use its own ConfigSpace like every
other persistent object. Firmware-image partition I/O remains outside this
NVS backend.
