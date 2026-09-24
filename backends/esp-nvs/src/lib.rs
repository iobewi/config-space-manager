#![no_std]

//! ESP NVS backend for config-space-manager.
//!
//! NVS semantics live here. Physical flash ownership is supplied by the
//! neutral esp-flash-access capability so this backend can coexist with
//! other flash consumers such as FiBeWI without exposing NVS outside.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use config_space_manager::{Budget, ConfigBackend, Snapshot};
use embedded_storage::nor_flash::{ErrorType, MultiwriteNorFlash, NorFlash, ReadNorFlash};
use esp_flash_access::{EspFlash, SharedFlash};
use esp_nvs::error::Error as NvsError;
use esp_nvs::platform::Crc;
use esp_nvs::Nvs;
use log::warn;

pub use esp_nvs::{ENTRIES_PER_PAGE, ITEM_SIZE, MAX_BLOB_DATA_PER_PAGE};

const NAMESPACE: esp_nvs::Key = esp_nvs::Key::from_str("cfg_space");
const HEALTH_NAMESPACE: esp_nvs::Key = esp_nvs::Key::from_str("cfg_health");
const HEALTH_KEY: esp_nvs::Key = esp_nvs::Key::from_str("canary");
const MAGIC: [u8; 4] = *b"CSM1";
const FLAG_PRESENT: u8 = 0x01;
const HEADER_LEN: usize = 4 + 8 + 1;
const MAX_NVS_KEY_LEN: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvsPartition {
    pub offset: usize,
    pub size: usize,
}

impl NvsPartition {
    pub const fn new(offset: usize, size: usize) -> Self {
        Self { offset, size }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvsConfigError {
    Unavailable,
    Write,
    InvalidSpace,
    CorruptRecord,
    GenerationOverflow,
}

struct NvsFlash<'a>(&'a mut EspFlash);

impl ErrorType for NvsFlash<'_> {
    type Error = <EspFlash as ErrorType>::Error;
}

impl ReadNorFlash for NvsFlash<'_> {
    const READ_SIZE: usize = <EspFlash as ReadNorFlash>::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl NorFlash for NvsFlash<'_> {
    const WRITE_SIZE: usize = <EspFlash as NorFlash>::WRITE_SIZE;
    const ERASE_SIZE: usize = <EspFlash as NorFlash>::ERASE_SIZE;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0.erase(from, to)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write(offset, bytes)
    }
}

impl MultiwriteNorFlash for NvsFlash<'_> {}

impl Crc for NvsFlash<'_> {
    fn crc32(init: u32, data: &[u8]) -> u32 {
        <esp_storage::FlashStorage<'static> as Crc>::crc32(init, data)
    }
}

static HEALTHY: AtomicBool = AtomicBool::new(true);

#[derive(Clone, Copy)]
pub struct NvsConfigBackend {
    flash: &'static SharedFlash,
    partition: NvsPartition,
    capacity_units: usize,
}

impl NvsConfigBackend {
    pub async fn new(
        flash: &'static SharedFlash,
        partition: NvsPartition,
    ) -> Result<Self, NvsConfigError> {
        let capacity_units = {
            let mut flash = flash.lock().await;
            let mut nvs = Nvs::new(partition.offset, partition.size, NvsFlash(&mut *flash))
                .map_err(|e| {
                    warn!("NVS unavailable: {e:?}");
                    HEALTHY.store(false, Ordering::Relaxed);
                    NvsConfigError::Unavailable
                })?;
            let stats = nvs.statistics().map_err(|e| {
                warn!("Failed to read NVS statistics: {e:?}");
                HEALTHY.store(false, Ordering::Relaxed);
                NvsConfigError::Write
            })?;
            let reclaimable = (stats.entries_overall.empty as usize)
                .saturating_add(stats.entries_overall.erased as usize);
            reclaimable.saturating_sub(ENTRIES_PER_PAGE)
        };
        HEALTHY.store(true, Ordering::Relaxed);
        Ok(Self { flash, partition, capacity_units })
    }

    pub fn is_healthy(&self) -> bool {
        HEALTHY.load(Ordering::Relaxed)
    }

    pub async fn self_check(&self) -> bool {
        const VALUE: u8 = 0xA5;
        let ok = self.with_nvs(|nvs| {
            nvs.set(&HEALTH_NAMESPACE, &HEALTH_KEY, VALUE).map_err(|_| NvsConfigError::Write)?;
            let read_back = nvs.get::<u8>(&HEALTH_NAMESPACE, &HEALTH_KEY).map_err(|_| NvsConfigError::Write)? == VALUE;
            nvs.delete(&HEALTH_NAMESPACE, &HEALTH_KEY).map_err(|_| NvsConfigError::Write)?;
            Ok(read_back)
        }).await.unwrap_or(false);
        HEALTHY.store(ok, Ordering::Relaxed);
        ok
    }

    async fn with_nvs<R>(
        &self,
        f: impl FnOnce(&mut Nvs<NvsFlash<'_>>) -> Result<R, NvsConfigError>,
    ) -> Result<R, NvsConfigError> {
        let mut flash = self.flash.lock().await;
        let mut nvs = Nvs::new(self.partition.offset, self.partition.size, NvsFlash(&mut *flash))
            .map_err(|e| {
                warn!("NVS unavailable: {e:?}");
                HEALTHY.store(false, Ordering::Relaxed);
                NvsConfigError::Unavailable
            })?;
        let result = f(&mut nvs);
        if result.is_err() {
            HEALTHY.store(false, Ordering::Relaxed);
        }
        result
    }

    fn valid_space_name(space: &str) -> bool {
        !space.is_empty()
            && space.len() <= MAX_NVS_KEY_LEN
            && space.as_bytes().iter().all(|b| b.is_ascii() && *b != 0)
    }

    fn key(space: &str) -> Result<esp_nvs::Key, NvsConfigError> {
        if !Self::valid_space_name(space) {
            return Err(NvsConfigError::InvalidSpace);
        }
        Ok(esp_nvs::Key::from_slice(space.as_bytes()))
    }

    fn entries_for_blob(encoded_size: usize) -> Option<usize> {
        let data_entries = encoded_size.checked_add(ITEM_SIZE - 1)? / ITEM_SIZE;
        let chunks = encoded_size.checked_add(MAX_BLOB_DATA_PER_PAGE - 1)? / MAX_BLOB_DATA_PER_PAGE;
        data_entries.checked_add(chunks)?.checked_add(1)
    }

    fn encode_record(generation: u64, present: bool, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&generation.to_le_bytes());
        out.push(if present { FLAG_PRESENT } else { 0 });
        out.extend_from_slice(payload);
        out
    }

    fn decode_record(raw: &[u8]) -> Result<(u64, bool, &[u8]), NvsConfigError> {
        if raw.len() < HEADER_LEN || raw[..4] != MAGIC {
            return Err(NvsConfigError::CorruptRecord);
        }
        let mut generation = [0u8; 8];
        generation.copy_from_slice(&raw[4..12]);
        let generation = u64::from_le_bytes(generation);
        let flags = raw[12];
        if flags & !FLAG_PRESENT != 0 {
            return Err(NvsConfigError::CorruptRecord);
        }
        Ok((generation, flags & FLAG_PRESENT != 0, &raw[HEADER_LEN..]))
    }

    async fn replace(&self, space: &str, present: bool, payload: &[u8]) -> Result<u64, NvsConfigError> {
        let key = Self::key(space)?;
        self.with_nvs(|nvs| {
            let generation = match nvs.get::<Vec<u8>>(&NAMESPACE, &key) {
                Ok(raw) => {
                    let (generation, _, _) = Self::decode_record(&raw)?;
                    generation.checked_add(1).ok_or(NvsConfigError::GenerationOverflow)?
                }
                Err(NvsError::NamespaceNotFound | NvsError::KeyNotFound) => 1,
                Err(e) => {
                    warn!("Failed to read blob {}: {e:?}", key.as_str());
                    return Err(NvsConfigError::Write);
                }
            };
            let encoded = Self::encode_record(generation, present, payload);
            nvs.set(&NAMESPACE, &key, encoded.as_slice()).map_err(|e| {
                warn!("Failed to save blob {}: {e:?}", key.as_str());
                NvsConfigError::Write
            })?;
            Ok(generation)
        }).await
    }
}

impl ConfigBackend for NvsConfigBackend {
    type Error = NvsConfigError;

    fn capacity_units(&self) -> usize { self.capacity_units }

    fn reservation_units(&self, space: &str, budget: Budget) -> Option<usize> {
        if !Self::valid_space_name(space) { return None; }
        let encoded_size = HEADER_LEN.checked_add(budget.max_bytes())?;
        let one_version = Self::entries_for_blob(encoded_size)?;
        one_version.checked_mul(2)
    }

    async fn load(&self, space: &str) -> Result<Option<Snapshot>, Self::Error> {
        let key = Self::key(space)?;
        self.with_nvs(|nvs| match nvs.get::<Vec<u8>>(&NAMESPACE, &key) {
            Ok(raw) => {
                let (generation, present, payload) = Self::decode_record(&raw)?;
                Ok(present.then(|| Snapshot { generation, data: payload.to_vec() }))
            }
            Err(NvsError::NamespaceNotFound | NvsError::KeyNotFound) => Ok(None),
            Err(e) => {
                warn!("Failed to read blob {}: {e:?}", key.as_str());
                Err(NvsConfigError::Write)
            }
        }).await
    }

    async fn commit(&self, space: &str, data: &[u8]) -> Result<u64, Self::Error> {
        self.replace(space, true, data).await
    }

    async fn clear(&self, space: &str) -> Result<u64, Self::Error> {
        self.replace(space, false, &[]).await
    }
}
