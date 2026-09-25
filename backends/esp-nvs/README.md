# config-space-manager-esp-nvs

ESP NVS adapter for `config-space-manager`.

This crate owns **ConfigSpace semantics over NVS**, not the physical ESP flash.
The common hardware layer is provided by `esp-storage-manager`.

```text
ConfigManager / ConfigSpace
            │
            ▼
config-space-manager-esp-nvs
  framing / quotas / generations
            │
            ▼
esp-storage-manager
  shared flash + ESP NVS platform bridge
            │
            ▼
        ESP flash
```

Application code cannot address ConfigSpace's NVS namespaces or keys directly.
Persistent application state is represented as component-owned ConfigSpace
objects.

OTA transaction metadata may use its own ConfigSpace like any other persistent
object. Firmware-image partition I/O remains outside this backend and is handled
through the same common hardware storage layer by FiBeWI's ESP adapter.
