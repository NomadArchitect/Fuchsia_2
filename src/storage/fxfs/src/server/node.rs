// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::server::directory::FxDirectory,
    futures::future::poll_fn,
    std::{
        any::Any,
        collections::HashMap,
        sync::{Arc, Mutex, Weak},
        task::{Poll, Waker},
        vec::Vec,
    },
};

/// FxNode is a node in the filesystem hierarchy (either a file or directory).
pub trait FxNode: Any + Send + Sync + 'static {
    fn object_id(&self) -> u64;
    fn parent(&self) -> Option<Arc<FxDirectory>>;
    fn set_parent(&self, parent: Arc<FxDirectory>);
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static>;
}

struct PlaceholderInner {
    object_id: u64,
    waker_sequence: u64,
    wakers: Vec<Waker>,
}

struct Placeholder(Mutex<PlaceholderInner>);

impl FxNode for Placeholder {
    fn object_id(&self) -> u64 {
        self.0.lock().unwrap().object_id
    }
    fn parent(&self) -> Option<Arc<FxDirectory>> {
        unreachable!();
    }
    fn set_parent(&self, _parent: Arc<FxDirectory>) {
        unreachable!();
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
        self
    }
}

/// PlaceholderOwner is a reserved slot in the node cache.
pub struct PlaceholderOwner<'a> {
    inner: Arc<Placeholder>,
    committed: bool,
    cache: &'a NodeCache,
}

impl PlaceholderOwner<'_> {
    /// Commits a node to the cache, replacing the placeholder and unblocking any waiting callers.
    pub fn commit(mut self, node: &Arc<dyn FxNode>) {
        let this_object_id = self.inner.object_id();
        assert_eq!(node.object_id(), this_object_id);
        self.committed = true;
        self.cache.0.lock().unwrap().map.insert(this_object_id, Arc::downgrade(node));
    }
}

impl Drop for PlaceholderOwner<'_> {
    fn drop(&mut self) {
        let mut p = self.inner.0.lock().unwrap();
        if !self.committed {
            // If the placeholder is dropped before it was committed, remove the cache entry so that
            // another caller blocked in NodeCache::get_or_reserve can take the slot.
            self.cache.0.lock().unwrap().map.remove(&p.object_id);
        }
        for waker in p.wakers.drain(..) {
            waker.wake();
        }
    }
}

/// See NodeCache::get_or_reserve.
pub enum GetResult<'a> {
    Placeholder(PlaceholderOwner<'a>),
    Node(Arc<dyn FxNode>),
}

struct NodeCacheInner {
    map: HashMap<u64, Weak<dyn FxNode>>,
    next_waker_sequence: u64,
}

/// NodeCache is an in-memory cache of weak node references.
pub struct NodeCache(Mutex<NodeCacheInner>);

impl NodeCache {
    pub fn new() -> Self {
        Self(Mutex::new(NodeCacheInner { map: HashMap::new(), next_waker_sequence: 0 }))
    }

    /// Gets a node in the cache, or reserves a placeholder in the cache to fill.
    ///
    /// Only the first caller will receive a placeholder result; all callers after that will block
    /// until the placeholder is filled (or the placeholder is dropped, at which point the next
    /// caller would get a placeholder). Callers that receive a placeholder should later commit a
    /// node with NodeCache::commit.
    pub async fn get_or_reserve<'a>(&'a self, object_id: u64) -> GetResult<'a> {
        let mut waker_sequence = 0;
        let mut waker_index = 0;
        poll_fn(|cx| {
            let mut this = self.0.lock().unwrap();
            if let Some(node) = this.map.get(&object_id) {
                if let Some(node) = node.upgrade() {
                    if let Ok(placeholder) = node.clone().into_any().downcast::<Placeholder>() {
                        let mut inner = placeholder.0.lock().unwrap();
                        if inner.waker_sequence == waker_sequence {
                            inner.wakers[waker_index] = cx.waker().clone();
                        } else {
                            waker_index = inner.wakers.len();
                            waker_sequence = inner.waker_sequence;
                            inner.wakers.push(cx.waker().clone());
                        }
                        return Poll::Pending;
                    } else {
                        return Poll::Ready(GetResult::Node(node));
                    }
                }
            }
            this.next_waker_sequence += 1;
            let inner = Arc::new(Placeholder(Mutex::new(PlaceholderInner {
                object_id,
                waker_sequence: this.next_waker_sequence,
                wakers: vec![],
            })));
            this.map.insert(object_id, Arc::downgrade(&inner) as Weak<dyn FxNode>);
            Poll::Ready(GetResult::Placeholder(PlaceholderOwner {
                inner,
                committed: false,
                cache: self,
            }))
        })
        .await
    }

    /// Removes a node from the cache. Calling this on a placeholder is an error; instead, the
    /// placeholder should simply be dropped.
    pub fn remove(&self, object_id: u64) {
        self.0.lock().unwrap().map.remove(&object_id);
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::server::{
            directory::FxDirectory,
            node::{FxNode, GetResult, NodeCache},
        },
        fuchsia_async as fasync,
        std::{
            any::Any,
            sync::{
                atomic::{AtomicU64, Ordering},
                Arc, Mutex,
            },
            time::Duration,
        },
    };

    struct FakeNode(u64, Arc<NodeCache>);
    impl FxNode for FakeNode {
        fn object_id(&self) -> u64 {
            self.0
        }
        fn parent(&self) -> Option<Arc<FxDirectory>> {
            unreachable!();
        }
        fn set_parent(&self, _parent: Arc<FxDirectory>) {
            unreachable!();
        }
        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync + 'static> {
            self
        }
    }
    impl Drop for FakeNode {
        fn drop(&mut self) {
            self.1.remove(self.0);
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_drop_placeholder() {
        let cache = Arc::new(NodeCache::new());
        let object_id = 0u64;
        match cache.get_or_reserve(object_id).await {
            GetResult::Node(_) => panic!("Unexpected node"),
            GetResult::Placeholder(_) => {}
        };
        match cache.get_or_reserve(object_id).await {
            GetResult::Node(_) => panic!("Unexpected node"),
            GetResult::Placeholder(_) => {}
        };
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_simple() {
        let cache = Arc::new(NodeCache::new());
        let object_id = {
            let node = Arc::new(FakeNode(0, cache.clone()));
            match cache.get_or_reserve(node.object_id()).await {
                GetResult::Node(_) => panic!("Unexpected node"),
                GetResult::Placeholder(p) => {
                    p.commit(&(node.clone() as Arc<dyn FxNode>));
                }
            };
            match cache.get_or_reserve(node.object_id()).await {
                GetResult::Node(n) => assert_eq!(n.object_id(), node.object_id()),
                GetResult::Placeholder(_) => panic!("No node found"),
            };
            node.object_id()
        };
        match cache.get_or_reserve(object_id).await {
            GetResult::Node(_) => panic!("Unexpected node"),
            GetResult::Placeholder(_) => {}
        };
    }

    #[fasync::run(10, test)]
    async fn test_subsequent_callers_block() {
        let cache = Arc::new(NodeCache::new());
        let object_id = 0u64;
        let writes_to_cache = Arc::new(AtomicU64::new(0));
        let reads_from_cache = Arc::new(AtomicU64::new(0));
        let node = Arc::new(FakeNode(object_id, cache.clone()));
        let mut tasks = vec![];
        for _ in 0..10 {
            let node = node.clone();
            let cache = cache.clone();
            let object_id = object_id.clone();
            let writes_to_cache = writes_to_cache.clone();
            let reads_from_cache = reads_from_cache.clone();
            tasks.push(async move {
                match cache.get_or_reserve(object_id).await {
                    GetResult::Node(node) => {
                        reads_from_cache.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(node.object_id(), object_id);
                    }
                    GetResult::Placeholder(p) => {
                        writes_to_cache.fetch_add(1, Ordering::SeqCst);
                        // Add a delay to simulate doing some work (e.g. loading from disk).
                        fasync::Timer::new(Duration::from_millis(100)).await;
                        p.commit(&(node as Arc<dyn FxNode>));
                    }
                }
            });
        }
        for t in tasks {
            t.await;
        }
        assert_eq!(writes_to_cache.load(Ordering::SeqCst), 1);
        assert_eq!(reads_from_cache.load(Ordering::SeqCst), 9);
    }

    #[fasync::run(10, test)]
    async fn test_multiple_nodes() {
        const NUM_OBJECTS: usize = 5;
        const TASKS_PER_OBJECT: usize = 4;

        let cache = Arc::new(NodeCache::new());
        let writes = Arc::new(Mutex::new(vec![0u64; NUM_OBJECTS]));
        let reads = Arc::new(Mutex::new(vec![0u64; NUM_OBJECTS]));
        let mut tasks = vec![];
        let mut nodes = vec![];
        for object_id in 0..NUM_OBJECTS as u64 {
            nodes.push(Arc::new(FakeNode(object_id, cache.clone())));
        }

        for _ in 0..TASKS_PER_OBJECT {
            for node in &nodes {
                let node = node.clone();
                let cache = cache.clone();
                let writes = writes.clone();
                let reads = reads.clone();
                tasks.push(async move {
                    match cache.get_or_reserve(node.object_id()).await {
                        GetResult::Node(result) => {
                            assert_eq!(node.object_id(), result.object_id());
                            reads.lock().unwrap()[node.object_id() as usize] += 1;
                        }
                        GetResult::Placeholder(p) => {
                            writes.lock().unwrap()[node.object_id() as usize] += 1;
                            // Add a delay to simulate doing some work (e.g. loading from disk).
                            fasync::Timer::new(Duration::from_millis(100)).await;
                            p.commit(&(node as Arc<dyn FxNode>));
                        }
                    }
                });
            }
        }
        for t in tasks {
            t.await;
        }
        assert_eq!(*writes.lock().unwrap(), vec![1u64; NUM_OBJECTS]);
        assert_eq!(*reads.lock().unwrap(), vec![TASKS_PER_OBJECT as u64 - 1; NUM_OBJECTS]);
    }
}
