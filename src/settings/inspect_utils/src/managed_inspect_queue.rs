// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_inspect::Node;
use fuchsia_inspect_derive::{AttachError, Inspect, WithInspect};
use std::collections::VecDeque;

/// A queue that wraps an inspect node and attaches all inserted values to the node.
///
/// This class can either be explicitly given an inspect node through
/// [ManagedInspectQueue::with_node] or can create its own inspect node when included in a struct
/// that derives Inspect or when [ManagedInspectQueue::with_inspect] is called. ManagedInspectQueue
/// will only keep the last [size_limit] items.
#[derive(Default)]
pub struct ManagedInspectQueue<V> {
    items: VecDeque<V>,
    // Required because VecDeque::with_capacity doesn't necessarily give the
    // exact capacity specified.
    size_limit: usize,
    inspect_node: Node,
}

impl<V> ManagedInspectQueue<V>
where
    for<'a> &'a mut V: Inspect,
{
    /// Creates a new [ManagedInspectQueue] that attaches inserted values to the given node.
    /// A size limit of 0 indicates an unlimited length.
    pub fn with_node(node: Node, size_limit: usize) -> Self {
        Self { items: VecDeque::with_capacity(size_limit), size_limit, inspect_node: node }
    }

    /// Sets the max number of elements allowed in the queue. If the size is smaller than the
    /// [new_size_limit], the oldest elements will be dropped until it is the right size. A size
    /// limit of 0 indicates an unlimited length.
    #[cfg(test)]
    fn set_size_limit(&mut self, new_size_limit: usize) {
        while self.items.len() > new_size_limit {
            let _ = self.items.pop_front();
        }
        self.size_limit = new_size_limit;
    }

    /// Returns a mutable reference to the underlying queue.
    #[cfg(test)]
    pub fn items_mut(&mut self) -> &mut VecDeque<V> {
        &mut self.items
    }

    /// Returns a reference to the [ManagedInspectQueue]'s node.
    pub fn inspect_node(&self) -> &Node {
        &self.inspect_node
    }

    /// Inserts the given value into the queue and attaches it to the inspect tree. If the new
    /// size of the queue is over capacity, the oldest value is removed.
    pub fn push(&mut self, key: &str, item: V) {
        // If the queue is over capacity, remove one. If the size limit is 0, assume no limit.
        if self.items.len() == self.size_limit && self.size_limit != 0 {
            let _ = self.items.pop_front();
        }
        let node = &self.inspect_node;
        self.items
            .push_back(item.with_inspect(node, key).expect("Failed to attach new queue entry."));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }
}

impl<V> Inspect for &mut ManagedInspectQueue<V>
where
    for<'a> &'a mut V: Inspect,
{
    fn iattach(self, parent: &Node, name: impl AsRef<str>) -> Result<(), AttachError> {
        self.inspect_node = parent.create_child(name.as_ref());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::managed_inspect_queue::ManagedInspectQueue;
    use fuchsia_inspect::{self as inspect, assert_data_tree, Node};
    use fuchsia_inspect_derive::{IValue, Inspect, WithInspect};

    #[derive(Default, Inspect)]
    struct TestInspectWrapper {
        inspect_node: Node,
        queue: ManagedInspectQueue<TestInspectItem>,
    }

    #[derive(Default, Inspect)]
    struct TestInspectItem {
        inspect_node: Node,
        id: IValue<u64>,
    }

    impl TestInspectItem {
        fn new(id: u64) -> Self {
            Self { inspect_node: Node::default(), id: id.into() }
        }
    }

    // Test that a queue with less items than its capacity has the correct
    // inspect and queue content.
    #[test]
    fn test_queue_under_capacity() {
        let inspector = inspect::Inspector::new();
        let mut wrapper = TestInspectWrapper::default()
            .with_inspect(inspector.root(), "inspect_wrapper")
            .expect("failed to create TestInspectWrapper inspect node");
        wrapper.queue.set_size_limit(2);

        let test_val_1 = TestInspectItem::new(6);
        let test_val_2 = TestInspectItem::new(5);

        wrapper.queue.push("0", test_val_1);
        wrapper.queue.push("1", test_val_2);

        assert_data_tree!(inspector, root: {
            inspect_wrapper: {
                queue: {
                    "0": {
                        "id": 6_u64,
                    },
                    "1": {
                        "id": 5_u64,
                    },
                },
            }
        });
        assert_eq!(wrapper.queue.len(), 2);
    }

    // Test that a queue with more items than its capacity has the correct
    // inspect and queue content.
    #[test]
    fn test_queue_over_capacity() {
        let inspector = inspect::Inspector::new();
        let mut wrapper = TestInspectWrapper::default()
            .with_inspect(inspector.root(), "inspect_wrapper")
            .expect("failed to create TestInspectWrapper inspect node");
        wrapper.queue.set_size_limit(2);

        let test_val_1 = TestInspectItem::new(6);
        let test_val_2 = TestInspectItem::new(5);
        let test_val_3 = TestInspectItem::new(4);

        wrapper.queue.push("0", test_val_1);
        wrapper.queue.push("1", test_val_2);
        wrapper.queue.push("2", test_val_3);

        assert_data_tree!(inspector, root: {
            inspect_wrapper: {
                queue: {
                    "1": {
                        "id": 5_u64,
                    },
                    "2": {
                        "id": 4_u64,
                    },
                },
            }
        });
        assert_eq!(wrapper.queue.len(), 2);
    }

    // Test that when setting the size limit smaller than the current number of elements, the
    // excess elements are dropped.
    #[test]
    fn test_size_limit() {
        let inspector = inspect::Inspector::new();
        let mut wrapper = TestInspectWrapper::default()
            .with_inspect(inspector.root(), "inspect_wrapper")
            .expect("failed to create TestInspectWrapper inspect node");

        let test_val_1 = TestInspectItem::new(6);
        let test_val_2 = TestInspectItem::new(5);
        let test_val_3 = TestInspectItem::new(4);

        wrapper.queue.push("0", test_val_1);
        wrapper.queue.push("1", test_val_2);
        wrapper.queue.push("2", test_val_3);
        wrapper.queue.set_size_limit(1);

        assert_data_tree!(inspector, root: {
            inspect_wrapper: {
                queue: {
                    "2": {
                        "id": 4_u64,
                    },
                },
            }
        });
        assert_eq!(wrapper.queue.len(), 1);
    }

    // Tests that removing items from the queue automatically removes them from inspect.
    #[test]
    fn test_queue_remove() {
        let inspector = inspect::Inspector::new();

        let mut queue = ManagedInspectQueue::<IValue<String>>::with_node(
            inspector.root().create_child("managed_node"),
            10,
        );

        let _ = queue.push("0", "value1".to_string().into());
        let _ = queue.push("1", "value2".to_string().into());
        let _ = queue.items_mut().remove(0);

        assert_data_tree!(inspector, root: {
            managed_node: {
                "1": "value2"
            }
        });
    }
}
