// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde::Deserialize;
use {
    anyhow::format_err,
    ieee80211::Ssid,
    log::{error, warn},
    parking_lot::{Mutex, MutexGuard},
    serde::Serialize,
    serde_json,
    std::{
        collections::HashMap,
        fs, io, mem,
        path::{Path, PathBuf},
    },
};

pub const KNOWN_NETWORKS_PATH: &str = "/data/known_networks.json";
pub const TMP_KNOWN_NETWORKS_PATH: &str = "/data/known_networks.json.tmp";

#[derive(Clone, Debug, PartialEq)]
pub struct KnownEss {
    pub password: Vec<u8>,
}

type EssMap = HashMap<Vec<u8>, KnownEss>;
pub struct KnownEssStore {
    storage_path: PathBuf,
    tmp_storage_path: PathBuf,
    ess_by_ssid: Mutex<EssMap>,
}

// Warning: changing this struct will break persistence
#[derive(Deserialize)]
pub struct EssJsonRead {
    pub ssid: Vec<u8>,
    pub password: Vec<u8>,
}

// Warning: changing this struct will break persistence
#[derive(Serialize)]
struct EssJsonWrite<'a> {
    ssid: &'a [u8],
    password: &'a [u8],
}

impl KnownEssStore {
    pub fn new_with_paths(
        storage_path: PathBuf,
        tmp_storage_path: PathBuf,
    ) -> Result<Self, anyhow::Error> {
        let ess_list: Vec<EssJsonRead> = match fs::File::open(&storage_path) {
            Ok(file) => match serde_json::from_reader(io::BufReader::new(file)) {
                Ok(list) => list,
                Err(e) => {
                    error!(
                        "Failed to parse the list of known wireless networks from JSONin {}: {}. \
                         Starting with an empty list.",
                        storage_path.display(),
                        e
                    );
                    fs::remove_file(&storage_path).map_err(|e| {
                        format_err!("Failed to delete {}: {}", storage_path.display(), e)
                    })?;
                    Vec::new()
                }
            },
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Vec::new(),
                _ => return Err(format_err!("Failed to open {}: {}", storage_path.display(), e)),
            },
        };
        let mut ess_by_ssid = HashMap::with_capacity(ess_list.len());
        for ess in ess_list {
            if let Some(_) = ess_by_ssid.insert(ess.ssid, KnownEss { password: ess.password }) {
                warn!("Duplicate ssid found in ess list");
            };
        }
        let ess_by_ssid = Mutex::new(ess_by_ssid);
        Ok(KnownEssStore { storage_path, tmp_storage_path, ess_by_ssid })
    }

    // still used in the below tests
    #[cfg(test)]
    pub fn lookup(&self, ssid: &[u8]) -> Option<KnownEss> {
        self.ess_by_ssid.lock().get(ssid).map(Clone::clone)
    }

    pub fn store(&self, ssid: Ssid, ess: KnownEss) -> Result<(), anyhow::Error> {
        let mut guard = self.ess_by_ssid.lock();
        // Even if writing into the file fails, it is still okay
        // to modify the in-memory map. We are not too worried about consistency here.
        if let Some(_) = guard.insert(ssid.to_vec(), ess) {
            warn!("Overwriting prior entry for ssid");
        };
        self.write(guard)
    }

    // Remove the network from persistent storage - only used by SavedNetworksManager to support
    // legacy storage temporarily.
    pub fn remove(&self, ssid: Vec<u8>, ess: Vec<u8>) -> Result<(), anyhow::Error> {
        let mut guard = self.ess_by_ssid.lock();
        if let Some(known_ess) = guard.get(&ssid) {
            if known_ess.password == ess {
                let _ = guard.remove(&ssid);
                self.write(guard)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn clear(&self) -> Result<(), anyhow::Error> {
        let mut guard = self.ess_by_ssid.lock();
        guard.clear();
        self.write(guard)
    }

    //still used in the below tests
    #[cfg(test)]
    pub fn known_network_count(&self) -> usize {
        self.ess_by_ssid.lock().len()
    }

    fn write(&self, guard: MutexGuard<'_, EssMap>) -> Result<(), anyhow::Error> {
        let temp_file = TempFile::create(&self.tmp_storage_path)?;
        let mut list = Vec::with_capacity(guard.len());
        for (ssid, ess) in guard.iter() {
            list.push(EssJsonWrite { ssid: &ssid[..], password: &ess.password[..] })
        }
        serde_json::to_writer(io::BufWriter::new(&temp_file.file), &list).map_err(|e| {
            format_err!("Failed to serialize JSON into {}: {}", self.tmp_storage_path.display(), e)
        })?;
        temp_file.close_and_rename(&self.storage_path).map_err(|e| {
            format_err!(
                "Failed to rename {} into {}: {}",
                self.tmp_storage_path.display(),
                self.storage_path.display(),
                e
            )
        })?;
        // Ensure that the lock is held until we are done writing
        let _ = &guard;
        Ok(())
    }
}

struct TempPath<'a> {
    path: &'a Path,
}

impl<'a> Drop for TempPath<'a> {
    fn drop(&mut self) {
        fs::remove_file(self.path).unwrap_or_else(|e| {
            error!("Failed to delete temporary file {}: {}", self.path.display(), e)
        });
    }
}

struct TempFile<'a> {
    path: TempPath<'a>,
    file: fs::File,
}

impl<'a> TempFile<'a> {
    pub fn create(path: &'a Path) -> Result<Self, anyhow::Error> {
        let file = fs::File::create(path)
            .map_err(|e| format_err!("Failed to open {} for writing: {}", path.display(), e))?;
        let path = TempPath { path };
        Ok(TempFile { path, file })
    }

    pub fn close_and_rename(self, new_name: &Path) -> Result<(), anyhow::Error> {
        mem::drop(self.file);
        fs::rename(&self.path.path, new_name)?;
        mem::forget(self.path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile;

    const STORE_JSON_PATH: &str = "store.json";

    #[fuchsia::test]
    fn store_and_lookup() {
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");

        // Expect the store to be constructed successfully even if the file doesn't
        // exist yet
        let store = create_ess_store(temp_dir.path());

        assert_eq!(None, store.lookup(b"foo"));
        assert_eq!(0, store.known_network_count());
        store.store(Ssid::from("foo"), ess(b"qwerty")).expect("storing 'foo' failed");
        assert_eq!(Some(ess(b"qwerty")), store.lookup(b"foo"));
        assert_eq!(1, store.known_network_count());
        store.store(Ssid::from("foo"), ess(b"12345")).expect("storing 'foo' again failed");
        assert_eq!(Some(ess(b"12345")), store.lookup(b"foo"));
        assert_eq!(1, store.known_network_count());

        // Make sure that storage is persistent
        let store = create_ess_store(temp_dir.path());
        assert_eq!(Some(ess(b"12345")), store.lookup(b"foo"));
        assert_eq!(1, store.known_network_count());

        // Make sure that overwriting the existing file works
        store.store(Ssid::from("bar"), ess(b"zxcvb")).expect("storing 'bar' failed");
        let store = create_ess_store(temp_dir.path());
        assert_eq!(Some(ess(b"12345")), store.lookup(b"foo"));
        assert_eq!(Some(ess(b"zxcvb")), store.lookup(b"bar"));
        assert_eq!(2, store.known_network_count());
    }

    #[fuchsia::test]
    fn unwrap_or_else_from_bad_file() {
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let path = temp_dir.path().join(STORE_JSON_PATH);
        let mut file = fs::File::create(&path).expect("failed to open file for writing");
        // Write invalid JSON and close the file
        assert_eq!(file.write(b"{").expect("failed to write broken json into file"), 1);
        mem::drop(file);
        assert!(path.exists());

        // Constructing an EssStore should still succeed,
        // but the invalid file should be gone now
        let store = create_ess_store(temp_dir.path());
        assert!(!path.exists());

        // Writing an entry should create the file
        store.store(Ssid::from("foo"), ess(b"qwerty")).expect("storing 'foo' failed");
        assert!(path.exists());
    }

    #[fuchsia::test]
    fn bail_if_path_is_bad() {
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let store = KnownEssStore::new_with_paths(
            PathBuf::from("/dev/null/foo"),
            temp_dir.path().join("store.json.tmp"),
        )
        .expect("Failed to create a KnownEssStore");

        let e = store.store(Ssid::from("foo"), ess(b"qwerty")).expect_err("expected store to fail");
        assert!(
            e.to_string().contains("Failed to rename")
                && e.to_string().contains("into /dev/null/foo"),
            "error message was: {}",
            e
        );
    }

    #[fuchsia::test]
    fn clear() {
        let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");

        // Expect the store to be constructed successfully even if the file doesn't
        // exist yet
        let store = create_ess_store(temp_dir.path());

        store.store(Ssid::from("foo"), ess(b"qwerty")).expect("storing 'foo' failed");
        assert_eq!(Some(ess(b"qwerty")), store.lookup(b"foo"));
        assert_eq!(1, store.known_network_count());
        store.clear().expect("clearing store failed");
        assert_eq!(0, store.known_network_count());

        // Load store from the file to verify it is also gone from persistent storage
        let store = create_ess_store(temp_dir.path());
        assert_eq!(0, store.known_network_count());
    }

    fn create_ess_store(path: &Path) -> KnownEssStore {
        KnownEssStore::new_with_paths(path.join(STORE_JSON_PATH), path.join("store.json.tmp"))
            .expect("Failed to create a KnownEssStore")
    }

    fn ess(password: &[u8]) -> KnownEss {
        KnownEss { password: password.to_vec() }
    }
}
