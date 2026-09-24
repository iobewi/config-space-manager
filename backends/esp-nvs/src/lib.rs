#![no_std]

//! ESP NVS backend for config-space-manager.
//!
//! This crate is the physical ESP implementation of ConfigSpace persistence.
//! It owns the single FlashStorage instance, the cached NVS view, NVS capacity
//! accounting and ConfigSpace record framing.
//!
//! NVS remains private to this backend: application components receive only
//! ConfigSpace capabilities and cannot address namespaces or keys directly.
//!

extern crate alloc;

use alloc::vec::Vec;
use core::ptr::NonNull;

use config_space_manager::{Budget, ConfigBackend, Snapshot};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_storage::nor_flash::{ErrorType, MultiwriteNorFlash, NorFlash, ReadNorFlash};
use esp_hal::peripherals::FLASH;
use esp_nvs::error::Error as NvsError;
use esp_nvs::platform::Crc;
use esp_nvs::Nvs;
use esp_storage::FlashStorage;
use log::warn;
use static_cell::StaticCell;

pub use esp_nvs::{ENTRIES_PER_PAGE, ITEM_SIZE, MAX_BLOB_DATA_PER_PAGE};

const NAMESPACE: esp_nvs::Key = esp_nvs::Key::from_str("cfg_space");
const MAGIC: [u8; 4] = *b"CSM1";
const FLAG_PRESENT: u8 = 0x01;
const HEADER_LEN: usize = 4 + 8 + 1;
const MAX_NVS_KEY_LEN: usize = 15;

/// Flash range occupied by the ESP NVS partition.
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

#[derive(Clone, Copy)]
struct SharedFlash(NonNull<FlashStorage<'static>>);

// SAFETY: the pointer targets FLASH_STORAGE for the process lifetime. Every
// access is serialized through &mut PhysicalStorage behind STORAGE.
unsafe impl Send for SharedFlash {}

impl SharedFlash {
    fn flash(&mut self) -> &mut FlashStorage<'static> {
        // SAFETY: callers hold exclusive access to PhysicalStorage.
        unsafe { self.0.as_mut() }
    }
}

impl ErrorType for SharedFlash {
    type Error = <FlashStorage<'static> as ErrorType>::Error;
}

impl ReadNorFlash for SharedFlash {
    const READ_SIZE: usize = <FlashStorage<'static> as ReadNorFlash>::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.flash().read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        // SAFETY: immutable metadata access to the process-wide flash object.
        unsafe { self.0.as_ref() }.capacity()
    }
}

impl NorFlash for SharedFlash {
    const WRITE_SIZE: usize = <FlashStorage<'static> as NorFlash>::WRITE_SIZE;
    const ERASE_SIZE: usize = <FlashStorage<'static> as NorFlash>::ERASE_SIZE;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.flash().erase(from, to)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.flash().write(offset, bytes)
    }
}

impl MultiwriteNorFlash for SharedFlash {}

impl Crc for SharedFlash {
    fn crc32(init: u32, data: &[u8]) -> u32 {
        <FlashStorage<'static> as Crc>::crc32(init, data)
    }
}

struct PhysicalStorage {
    flash: SharedFlash,
    nvs: Option<Nvs<SharedFlash>>,
    partition: NvsPartition,
    healthy: bool,
}

impl PhysicalStorage {
    fn nvs(&mut self) -> Result<&mut Nvs<SharedFlash>, NvsConfigError> {
        if self.nvs.is_none() {
            match Nvs::new(self.partition.offset, self.partition.size, self.flash) {
                Ok(nvs) => self.nvs = Some(nvs),
                Err(e) => {
                    warn!("NVS unavailable: {e:?}");
                    self.healthy = false;
                    return Err(NvsConfigError::Unavailable);
                }
            }
        }
        self.nvs.as_mut().ok_or(NvsConfigError::Unavailable)
    }

    fn read_blob(
        &mut self,
        namespace: &esp_nvs::Key,
        key: &esp_nvs::Key,
    ) -> Result<Option<Vec<u8>>, NvsConfigError> {
        let nvs = self.nvs()?;
        match nvs.get(namespace, key) {
            Ok(value) => Ok(Some(value)),
            Err(NvsError::NamespaceNotFound | NvsError::KeyNotFound) => Ok(None),
            Err(e) => {
                warn!("Failed to read blob {}: {e:?}", key.as_str());
                self.nvs = None;
                self.healthy = false;
                Err(NvsConfigError::Write)
            }
        }
    }

    fn set_blob(
        &mut self,
        namespace: &esp_nvs::Key,
        key: &esp_nvs::Key,
        value: &[u8],
    ) -> Result<(), NvsConfigError> {
        let nvs = self.nvs()?;
        nvs.set(namespace, key, value).map_err(|e| {
            warn!("Failed to save blob {}: {e:?}", key.as_str());
            self.nvs = None;
            self.healthy = false;
            NvsConfigError::Write
        })
    }

    fn statistics(&mut self) -> Result<esp_nvs::NvsStatistics, NvsConfigError> {
        let nvs = self.nvs()?;
        match nvs.statistics() {
            Ok(stats) => Ok(stats),
            Err(e) => {
                warn!("Failed to read NVS statistics: {e:?}");
                self.nvs = None;
                self.healthy = false;
                Err(NvsConfigError::Write)
            }
        }
    }
}

static FLASH_STORAGE: StaticCell<FlashStorage<'static>> = StaticCell::new();
static STORAGE: StaticCell<Mutex<CriticalSectionRawMutex, PhysicalStorage>> = StaticCell::new();

type SharedPhysicalStorage = Mutex<CriticalSectionRawMutex, PhysicalStorage>;

/// ESP implementation of ConfigBackend.
///
/// Construct exactly once per firmware image. The underlying StaticCells make
/// accidental duplicate physical flash ownership fail immediately.
#[derive(Clone, Copy)]
pub struct NvsConfigBackend {
    storage: &'static SharedPhysicalStorage,
    capacity_units: usize,
}

impl NvsConfigBackend {
    pub async fn new(
        flash: FLASH<'static>,
        partition: NvsPartition,
    ) -> Result<Self, NvsConfigError> {
        let flash = FLASH_STORAGE.init(FlashStorage::new(flash));
        let shared_flash = SharedFlash(NonNull::from(flash));
        let storage = STORAGE.init(Mutex::new(PhysicalStorage {
            flash: shared_flash,
            nvs: None,
            partition,
            healthy: true,
        }));

        let stats = storage.lock().await.statistics()?;
        let reclaimable = (stats.entries_overall.empty as usize)
            .saturating_add(stats.entries_overall.erased as usize);
        let capacity_units = reclaimable.saturating_sub(ENTRIES_PER_PAGE);

        Ok(Self {
            storage,
            capacity_units,
        })
    }

    /// Last known NVS health. This does not perform a flash write.
    pub async fn is_healthy(&self) -> bool {
        self.storage.lock().await.healthy
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
        let chunks =
            encoded_size.checked_add(MAX_BLOB_DATA_PER_PAGE - 1)? / MAX_BLOB_DATA_PER_PAGE;
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

    async fn replace(
        &self,
        space: &str,
        present: bool,
        payload: &[u8],
    ) -> Result<u64, NvsConfigError> {
        let key = Self::key(space)?;
        let mut storage = self.storage.lock().await;

        let generation = match storage.read_blob(&NAMESPACE, &key)? {
            None => 1,
            Some(raw) => {
                let (generation, _, _) = Self::decode_record(&raw)?;
                generation
                    .checked_add(1)
                    .ok_or(NvsConfigError::GenerationOverflow)?
            }
        };

        let encoded = Self::encode_record(generation, present, payload);
        storage.set_blob(&NAMESPACE, &key, &encoded)?;
        Ok(generation)
    }
}

impl ConfigBackend for NvsConfigBackend {
    type Error = NvsConfigError;

    fn capacity_units(&self) -> usize {
        self.capacity_units
    }

    fn reservation_units(&self, space: &str, budget: Budget) -> Option<usize> {
        if !Self::valid_space_name(space) {
            return None;
        }

        let encoded_size = HEADER_LEN.checked_add(budget.max_bytes())?;
        let one_version = Self::entries_for_blob(encoded_size)?;
        one_version.checked_mul(2)
    }

    async fn load(&self, space: &str) -> Result<Option<Snapshot>, Self::Error> {
        let key = Self::key(space)?;
        let raw = self.storage.lock().await.read_blob(&NAMESPACE, &key)?;
        let Some(raw) = raw else {
            return Ok(None);
        };

        let (generation, present, payload) = Self::decode_record(&raw)?;
        if !present {
            return Ok(None);
        }

        Ok(Some(Snapshot {
            generation,
            data: payload.to_vec(),
        }))
    }

    async fn commit(&self, space: &str, data: &[u8]) -> Result<u64, Self::Error> {
        self.replace(space, true, data).await
    }

    async fn clear(&self, space: &str) -> Result<u64, Self::Error> {
        self.replace(space, false, &[]).await
    }
}
