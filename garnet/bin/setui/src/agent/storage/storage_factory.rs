// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::device_storage::{DeviceStorage, DeviceStorageAccess};
use crate::inspect::stash_logger::StashInspectLogger;
use anyhow::{format_err, Error};
use fidl::endpoints::create_proxy;
use fidl_fuchsia_stash::StoreProxy;
use futures::lock::Mutex;
use std::{collections::HashSet, sync::Arc};

/// `DeviceStorageFactory` abstracts over how to initialize and retrieve the `DeviceStorage`
/// instance.
#[async_trait::async_trait]
pub trait DeviceStorageFactory {
    /// Initialize the storage to be able to manage storage for objects of type T.
    /// This will return an Error once `get_store` is called the first time.
    async fn initialize<T>(&self) -> Result<(), Error>
    where
        T: DeviceStorageAccess;

    /// Retrieve the store singleton instance.
    async fn get_store(&self) -> Arc<DeviceStorage>;
}

/// The state of the factory. Only one state can be active at a time because once
/// the [`DeviceStorage`] is created, there's no way to change the keys, so there's
/// no need to keep the set of keys anymore.
enum InitializationState {
    /// This represents the state of the factory before the first request to get
    /// [`DeviceStorage`]. It maintains a list of all keys that might be used for
    /// storage.
    Initializing(HashSet<&'static str>),
    /// This represents the initialized state. When this is active, it is no longer
    /// possible to add new storage keys to [`DeviceStorage`].
    Initialized(Arc<DeviceStorage>),
}

impl InitializationState {
    /// Construct the default `InitializationState`.
    fn new() -> Self {
        Self::Initializing(HashSet::new())
    }
}

/// Factory that vends out storage.
pub struct StashDeviceStorageFactory {
    store: StoreProxy,
    device_storage_cache: Mutex<InitializationState>,
    inspect_handle: Arc<Mutex<StashInspectLogger>>,
}

impl StashDeviceStorageFactory {
    /// Construct a new instance of `StashDeviceStorageFactory`.
    pub fn new(
        identity: &str,
        store: StoreProxy,
        inspect_handle: Arc<Mutex<StashInspectLogger>>,
    ) -> StashDeviceStorageFactory {
        store.identify(identity).expect("was not able to identify with stash");
        StashDeviceStorageFactory {
            store,
            device_storage_cache: Mutex::new(InitializationState::new()),
            inspect_handle,
        }
    }

    // Speeds up compilation by not needing to monomorphize this code for all T's.
    async fn initialize_storage(&self, keys: &'static [&'static str]) -> Result<(), Error> {
        match &mut *self.device_storage_cache.lock().await {
            InitializationState::Initializing(initial_keys) => {
                for &key in keys {
                    let _ = initial_keys.insert(key);
                }
                Ok(())
            }
            InitializationState::Initialized(_) => {
                Err(format_err!("Cannot initialize an already accessed device storage"))
            }
        }
    }
}

#[async_trait::async_trait]
impl DeviceStorageFactory for StashDeviceStorageFactory {
    async fn initialize<T>(&self) -> Result<(), Error>
    where
        T: DeviceStorageAccess,
    {
        self.initialize_storage(T::STORAGE_KEYS).await
    }

    async fn get_store(&self) -> Arc<DeviceStorage> {
        let initialization = &mut *self.device_storage_cache.lock().await;
        match initialization {
            InitializationState::Initializing(initial_keys) => {
                let device_storage = Arc::new(DeviceStorage::with_stash_proxy(
                    initial_keys.drain(),
                    || {
                        let (accessor_proxy, server_end) =
                            create_proxy().expect("failed to create proxy for stash");
                        self.store
                            .create_accessor(false, server_end)
                            .expect("failed to create accessor for stash");
                        accessor_proxy
                    },
                    Arc::clone(&self.inspect_handle),
                ));
                *initialization = InitializationState::Initialized(Arc::clone(&device_storage));
                device_storage
            }
            InitializationState::Initialized(device_storage) => Arc::clone(device_storage),
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::sync::Arc;

    use fidl_fuchsia_stash::{
        StoreAccessorMarker, StoreAccessorProxy, StoreAccessorRequest, Value,
    };
    use fuchsia_async as fasync;
    use futures::lock::Mutex;
    use futures::prelude::*;

    use crate::agent::storage::device_storage::DeviceStorageCompatible;
    use crate::inspect::stash_logger::StashInspectLoggerHandle;

    use super::*;

    #[derive(PartialEq)]
    pub(crate) enum StashAction {
        Get,
        Flush,
        Set,
    }

    pub(crate) struct StashStats {
        actions: Vec<StashAction>,
    }

    impl StashStats {
        pub(crate) fn new() -> Self {
            StashStats { actions: Vec::new() }
        }

        pub(crate) fn record(&mut self, action: StashAction) {
            self.actions.push(action);
        }
    }

    /// Storage that does not write to disk, for testing.
    pub(crate) struct InMemoryStorageFactory {
        initial_data: HashMap<&'static str, String>,
        device_storage_cache: Mutex<InitializationState>,
        inspect_handle: Arc<Mutex<StashInspectLogger>>,
    }

    impl Default for InMemoryStorageFactory {
        fn default() -> Self {
            Self::new()
        }
    }

    const INITIALIZATION_ERROR: &str =
        "Cannot initialize an already accessed device storage. Make \
        sure you're not retrieving a DeviceStorage before passing InMemoryStorageFactory to an \
        EnvironmentBuilder. That must be done after. If you need initial data, use \
        InMemoryStorageFactory::with_initial_data";

    impl InMemoryStorageFactory {
        /// Constructs a new `InMemoryStorageFactory` with the ability to create a [`DeviceStorage`]
        /// that can only read and write to the storage keys passed in.
        pub(crate) fn new() -> Self {
            InMemoryStorageFactory {
                initial_data: HashMap::new(),
                device_storage_cache: Mutex::new(InitializationState::new()),
                inspect_handle: StashInspectLoggerHandle::new().logger,
            }
        }

        /// Constructs a new `InMemoryStorageFactory` with the data written to stash. This simulates
        /// the data existing in storage before the RestoreAgent reads it.
        pub(crate) fn with_initial_data<T>(data: &T) -> Self
        where
            T: DeviceStorageCompatible,
        {
            let mut map = HashMap::new();
            let _ = map.insert(T::KEY, serde_json::to_string(data).unwrap());
            InMemoryStorageFactory {
                initial_data: map,
                device_storage_cache: Mutex::new(InitializationState::new()),
                inspect_handle: StashInspectLoggerHandle::new().logger,
            }
        }

        /// Helper method to simplify setup for `InMemoryStorageFactory` in tests.
        pub(crate) async fn initialize_storage<T>(&self)
        where
            T: DeviceStorageCompatible,
        {
            self.initialize_storage_for_key(T::KEY).await;
        }

        async fn initialize_storage_for_key(&self, key: &'static str) {
            match &mut *self.device_storage_cache.lock().await {
                InitializationState::Initializing(initial_keys) => {
                    let _ = initial_keys.insert(key);
                }
                InitializationState::Initialized(_) => panic!("{}", INITIALIZATION_ERROR),
            }
        }

        async fn initialize_storage_for_keys(&self, keys: &'static [&'static str]) {
            match &mut *self.device_storage_cache.lock().await {
                InitializationState::Initializing(initial_keys) => {
                    for &key in keys {
                        let _ = initial_keys.insert(key);
                    }
                }
                InitializationState::Initialized(_) => panic!("{}", INITIALIZATION_ERROR),
            }
        }

        /// Retrieve the [`DeviceStorage`] singleton.
        pub(crate) async fn get_device_storage(&self) -> Arc<DeviceStorage> {
            let initialization = &mut *self.device_storage_cache.lock().await;
            match initialization {
                InitializationState::Initializing(initial_keys) => {
                    let mut device_storage = DeviceStorage::with_stash_proxy(
                        initial_keys.drain(),
                        || {
                            let (stash_proxy, _) = spawn_stash_proxy();
                            stash_proxy
                        },
                        Arc::clone(&self.inspect_handle),
                    );
                    device_storage.set_caching_enabled(false);
                    device_storage.set_debounce_writes(false);

                    // write initial data to storage
                    for (&key, data) in &self.initial_data {
                        device_storage
                            .write_str(key, data.clone())
                            .await
                            .expect("Failed to write initial data");
                    }

                    let device_storage = Arc::new(device_storage);
                    *initialization = InitializationState::Initialized(Arc::clone(&device_storage));
                    device_storage
                }
                InitializationState::Initialized(device_storage) => Arc::clone(device_storage),
            }
        }
    }

    #[async_trait::async_trait]
    impl DeviceStorageFactory for InMemoryStorageFactory {
        async fn initialize<T>(&self) -> Result<(), Error>
        where
            T: DeviceStorageAccess,
        {
            self.initialize_storage_for_keys(T::STORAGE_KEYS).await;
            Ok(())
        }

        async fn get_store(&self) -> Arc<DeviceStorage> {
            self.get_device_storage().await
        }
    }

    fn spawn_stash_proxy() -> (StoreAccessorProxy, Arc<Mutex<StashStats>>) {
        let (stash_proxy, mut stash_stream) =
            fidl::endpoints::create_proxy_and_stream::<StoreAccessorMarker>().unwrap();
        let stats = Arc::new(Mutex::new(StashStats::new()));
        let stats_clone = stats.clone();
        fasync::Task::spawn(async move {
            let mut stored_value: Option<Value> = None;
            let mut stored_key: Option<String> = None;

            while let Some(req) = stash_stream.try_next().await.unwrap() {
                #[allow(unreachable_patterns)]
                match req {
                    StoreAccessorRequest::GetValue { key, responder } => {
                        stats_clone.lock().await.record(StashAction::Get);
                        if let Some(key_string) = stored_key {
                            assert_eq!(key, key_string);
                        }
                        stored_key = Some(key);

                        responder.send(stored_value.as_mut()).unwrap();
                    }
                    StoreAccessorRequest::SetValue { key, val, control_handle: _ } => {
                        stats_clone.lock().await.record(StashAction::Set);
                        if let Some(key_string) = stored_key {
                            assert_eq!(key, key_string);
                        }
                        stored_key = Some(key);
                        stored_value = Some(val);
                    }
                    StoreAccessorRequest::Flush { responder } => {
                        stats_clone.lock().await.record(StashAction::Flush);
                        let _ = responder.send(&mut Ok(()));
                    }
                    _ => {}
                }
            }
        })
        .detach();
        (stash_proxy, stats)
    }
}
