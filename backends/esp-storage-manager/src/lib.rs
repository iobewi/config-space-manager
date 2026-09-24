#![no_std]

//! ESP NVS backend for config-space-manager.
//!
//! The core crate remains hardware-agnostic. This backend maps one logical
//! ConfigSpace to one opaque NVS blob backed by esp-storage-manager.
//!
//! It owns:
//! - ESP/NVS capacity accounting;
//! - ConfigSpace record framing and generation persistence;
//! - atomic whole-value replacement through one NVS blob;
//! - serialization of access to the single StorageManager.
//!
//! It deliberately owns no component schema or application policy.

extern crate alloc;

use alloc::vec::Vec;

use config_space_manager::{Budget, ConfigBackend, Snapshot};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_storage_manager::{
    ENTRIES_PER_PAGE, ITEM_SIZE, Key, MAX_BLOB_DATA_PER_PAGE, StorageError,
    StorageManager,
};

/// Shared process-wide ESP storage owner used by this backend.
pub type SharedStorageManager = Mutex<CriticalSectionRawMutex, StorageManager>;

const NAMESPACE: Key = Key::from_str("cfg_space");
const MAGIC: [u8; 4] = *b"CSM1";
const FLAG_PRESENT: u8 = 0x01;
const HEADER_LEN: usize = 4 + 8 + 1;
const MAX_NVS_KEY_LEN: usize = 15;

#[derive(Debug)]
pub enum NvsConfigError {
    Storage(StorageError),
    InvalidSpace,
    CorruptRecord,
    GenerationOverflow,
}

impl From<StorageError> for NvsConfigError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}

#[derive(Clone, Copy)]
pub struct NvsConfigBackend {
    storage: &'static SharedStorageManager,
    capacity_units: usize,
}

impl NvsConfigBackend {
    /// Opens the backend and snapshots capacity available to boot-time claims.
    ///
    /// One whole NVS page is deliberately withheld as compaction headroom.
    pub async fn new(
        storage: &'static SharedStorageManager,
    ) -> Result<Self, NvsConfigError> {
        let stats = storage.lock().await.nvs_statistics()?;
        let reclaimable = (stats.entries_overall.empty as usize)
            .saturating_add(stats.entries_overall.erased as usize);
        let capacity_units = reclaimable.saturating_sub(ENTRIES_PER_PAGE);

        Ok(Self {
            storage,
            capacity_units,
        })
    }

    fn valid_space_name(space: &str) -> bool {
        !space.is_empty()
            && space.len() <= MAX_NVS_KEY_LEN
            && space.as_bytes().iter().all(|b| b.is_ascii() && *b != 0)
    }

    fn key(space: &str) -> Result<Key, NvsConfigError> {
        if !Self::valid_space_name(space) {
            return Err(NvsConfigError::InvalidSpace);
        }
        Ok(Key::from_slice(space.as_bytes()))
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

        // Read-generation-write stays serialized under the same physical
        // storage lock, preventing lost generation increments.
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

        // Reserve both current and replacement versions so a successful claim
        // remains writable without borrowing another component's capacity.
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
        // Persist a tombstone instead of deleting the NVS key so generation
        // remains monotonic across clear/recreate cycles.
        self.replace(space, false, &[]).await
    }
}
