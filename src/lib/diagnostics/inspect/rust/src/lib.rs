// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! # fuchsia-inspect
//!
//! Components in Fuchsia may expose structured information about themselves conforming to the
//! [Inspect API][inspect]. This crate is the core library for writing inspect data in Rust
//! components.
//!
//! For a comprehensive guide on how to start using inspect, please refer to the
//! [codelab].
//!
//! ## Library concepts
//!
//! There's two types of inspect values: nodes and properties. These have the following
//! characteristics:
//!
//!   - A Node may have any number of key/value pairs called Properties.
//!   - A Node may have any number of children, which are also Nodes.
//!   - Properties and nodes are created under a parent node. Inspect is already initialized with a
//!     root node.
//!   - The key for a value in a Node is always a UTF-8 string, the value may be one of the
//!     supported types (a node or a property of any type).
//!   - Nodes and properties have strict ownership semantics. Whenever a node or property is
//!     created, it is written to the backing [VMO][inspect-vmo] and whenever it is dropped it is
//!     removed from the VMO.
//!   - Inspection is best effort, if an error occurs, no panic will happen and nodes and properties
//!     might become No-Ops. For example, when the VMO becomes full, any further creation of a
//!     property or a node will result in no changes in the VMO and a silent failure. However,
//!     mutation of existing properties in the VMO will continue to work.
//!   - All nodes and properties are thread safe.
//!
//! ### Creating vs Recording
//!
//! There are two functions each for initializing nodes and properties:
//!
//!   - `create_*`: returns the created node/property and it's up to the caller to handle its
//!     lifetime.
//!   - `record_*`: creates the node/property but doesn't return it and ties its lifetime to
//!     the node where the function was called.
//!
//! ### Lazy value support
//!
//! Lazy (or dynamic) values are values that are created on demand, this is, whenever they are read.
//! Unlike regular nodes, they don't take any space on the VMO until a reader comes and requests
//! its data.
//!
//! There's two ways of creating lazy values:
//!
//!   - **Lazy node**: creates a child node of root with the given name. The callback returns a
//!     future for an [`Inspector`][inspector] whose root node is spliced into the parent node when
//!     read.
//!   - **Lazy values**: works like the previous one, except that all properties and nodes under the
//!     future root node node are added directly as children of the parent node.
//!
//! ## Quickstart
//!
//! Add the following to your component main:
//!
//! ```rust
//! use fuchsia_inspect::component;
//! use fuchsia_component::server::ServiceFs;
//! use inspect_runtime;
//!
//! let mut fs = ServiceFs::new();
//! inspect_runtime::serve(component::inspector(), &mut fs)?;
//!
//! // Now you can create nodes and properties anywhere!
//! let child = component::inspector().root().create_child("foo");
//! child.record_uint("bar", 42);
//! ```
//!
//! [inspect]: https://fuchsia.dev/fuchsia-src/development/diagnostics/inspect
//! [codelab]: https://fuchsia.dev/fuchsia-src/development/diagnostics/inspect/codelab
//! [inspect-vmo]: https://fuchsia.dev/fuchsia-src/reference/diagnostics/inspect/vmo-format
//! [inspector]: Inspector

use {
    crate::{heap::Heap, state::State},
    anyhow,
    derivative::Derivative,
    diagnostics_hierarchy::testing::DiagnosticsHierarchyGetter,
    fuchsia_zircon::{self as zx, HandleBased},
    futures::{future::BoxFuture, prelude::*},
    inspect_format::{
        constants, {ArrayFormat, LinkNodeDisposition, PropertyFormat},
    },
    lazy_static::lazy_static,
    mapped_vmo::Mapping,
    parking_lot::Mutex,
    paste,
    std::{
        borrow::Cow,
        cmp::max,
        default::Default,
        fmt::Debug,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Weak,
        },
    },
    tracing::error,
};

#[cfg(test)]
use inspect_format::Block;

pub use diagnostics_hierarchy::{
    DiagnosticsHierarchy, ExponentialHistogramParams, LinearHistogramParams,
};
pub use testing::{assert_data_tree, tree_assertion};

// TODO: cleanup. For soft migration only.
pub use assert_data_tree as assert_inspect_tree;

pub use {crate::error::Error, crate::state::Stats};

pub mod component;
mod error;
pub mod health;
pub mod heap;
pub mod reader;
mod state;
pub mod stats;

pub mod testing {
    pub use diagnostics_hierarchy::{
        assert_data_tree,
        testing::{
            AnyProperty, DiagnosticsHierarchyGetter, HistogramAssertion, NonZeroUintProperty,
            PropertyAssertion, TreeAssertion,
        },
        tree_assertion,
    };
}

/// Directiory within the outgoing directory of a component where the diagnostics service should be
/// added.
pub const DIAGNOSTICS_DIR: &str = "diagnostics";

lazy_static! {
  // Suffix used for unique names.
  static ref UNIQUE_NAME_SUFFIX: AtomicUsize = AtomicUsize::new(0);
}

/// Root of the Inspect API. Through this API, further nodes can be created and inspect can be
/// served.
#[derive(Clone)]
pub struct Inspector {
    /// The root node.
    root_node: Arc<Node>,

    /// The VMO backing the inspector
    pub(in crate) vmo: Option<Arc<zx::Vmo>>,
}

impl DiagnosticsHierarchyGetter<String> for Inspector {
    fn get_diagnostics_hierarchy(&self) -> Cow<'_, DiagnosticsHierarchy> {
        let hierarchy = futures::executor::block_on(async move { reader::read(self).await })
            .expect("failed to get hierarchy");
        Cow::Owned(hierarchy)
    }
}

/// Holds a list of inspect types that won't change.
#[derive(Derivative)]
#[derivative(Debug, PartialEq, Eq)]
struct ValueList {
    #[derivative(PartialEq = "ignore")]
    #[derivative(Debug = "ignore")]
    values: Mutex<Option<InspectTypeList>>,
}

impl Default for ValueList {
    fn default() -> Self {
        ValueList::new()
    }
}

type InspectTypeList = Vec<Box<dyn InspectType>>;

impl ValueList {
    /// Creates a new empty value list.
    pub fn new() -> Self {
        Self { values: Mutex::new(None) }
    }

    /// Stores an inspect type that won't change.
    pub fn record(&self, value: impl InspectType + 'static) {
        let boxed_value = Box::new(value);
        let mut values_lock = self.values.lock();
        if let Some(ref mut values) = *values_lock {
            values.push(boxed_value);
        } else {
            *values_lock = Some(vec![boxed_value]);
        }
    }
}

impl Inspector {
    /// Initializes a new Inspect VMO object with the
    /// [`defalt maximum size`][constants::DEFAULT_VMO_SIZE_BYTES].
    pub fn new() -> Self {
        Inspector::new_with_size(constants::DEFAULT_VMO_SIZE_BYTES)
    }

    /// True if the Inspector was created successfully (it's not No-Op)
    pub fn is_valid(&self) -> bool {
        self.vmo.is_some() && self.root_node.is_valid()
    }

    /// Initializes a new Inspect VMO object with the given maximum size. If the
    /// given size is less than 4K, it will be made 4K which is the minimum size
    /// the VMO should have.
    pub fn new_with_size(max_size: usize) -> Self {
        match Inspector::new_root(max_size) {
            Ok((vmo, root_node)) => {
                Inspector { vmo: Some(Arc::new(vmo)), root_node: Arc::new(root_node) }
            }
            Err(e) => {
                error!("Failed to create root node. Error: {:?}", e);
                Inspector::new_no_op()
            }
        }
    }

    /// Returns a duplicate of the underlying VMO for this Inspector.
    ///
    /// The duplicated VMO will be read-only, and is suitable to send to clients over FIDL.
    pub fn duplicate_vmo(&self) -> Option<zx::Vmo> {
        self.vmo
            .as_ref()
            .map(|vmo| {
                vmo.duplicate_handle(zx::Rights::BASIC | zx::Rights::READ | zx::Rights::MAP).ok()
            })
            .unwrap_or(None)
    }

    /// Returns a VMO holding a copy of the data in this inspector.
    ///
    /// The copied VMO will be read-only.
    pub fn copy_vmo(&self) -> Option<zx::Vmo> {
        self.copy_vmo_data().and_then(|data| {
            if let Ok(vmo) = zx::Vmo::create(data.len() as u64) {
                vmo.write(&data, 0).ok().map(|_| vmo)
            } else {
                None
            }
        })
    }

    /// Returns a copy of the bytes stored in the VMO for this inspector.
    ///
    /// The output will be truncated to only those bytes that are needed to accurately read the
    /// stored data.
    pub fn copy_vmo_data(&self) -> Option<Vec<u8>> {
        self.root_node.inner.inner_ref().map(|inner_ref| inner_ref.state.copy_vmo_bytes())
    }

    /// Returns the root node of the inspect hierarchy.
    pub fn root(&self) -> &Node {
        &self.root_node
    }

    /// Takes a function to execute as under a single lock of the Inspect VMO. This function
    /// receives a reference to the root of the inspect hierarchy.
    pub fn atomic_update<F, R>(&self, update_fn: F) -> R
    where
        F: FnMut(&Node) -> R,
    {
        self.root().atomic_update(update_fn)
    }

    /// Creates a new No-Op inspector
    pub fn new_no_op() -> Self {
        Inspector { vmo: None, root_node: Arc::new(Node::new_no_op()) }
    }

    /// Allocates a new VMO and initializes it.
    fn new_root(max_size: usize) -> Result<(zx::Vmo, Node), Error> {
        let mut size = max(constants::MINIMUM_VMO_SIZE_BYTES, max_size);
        // If the size is not a multiple of 4096, round up.
        if size % constants::MINIMUM_VMO_SIZE_BYTES != 0 {
            size =
                (1 + size / constants::MINIMUM_VMO_SIZE_BYTES) * constants::MINIMUM_VMO_SIZE_BYTES;
        }
        let (mapping, vmo) = Mapping::allocate_with_name(size, "InspectHeap")
            .map_err(|status| Error::AllocateVmo(status))?;
        let heap = Heap::new(Arc::new(mapping)).map_err(|e| Error::CreateHeap(Box::new(e)))?;
        let state = State::create(heap).map_err(|e| Error::CreateState(Box::new(e)))?;
        Ok((vmo, Node::new_root(state)))
    }

    /// Creates an no-op inspector from the given Vmo. If the VMO is corrupted, reading can fail.
    fn no_op_from_vmo(vmo: Arc<zx::Vmo>) -> Inspector {
        Inspector { vmo: Some(vmo), root_node: Arc::new(Node::new_no_op()) }
    }

    pub(crate) fn state(&self) -> Option<State> {
        self.root().inner.inner_ref().map(|inner_ref| inner_ref.state.clone())
    }
}

/// Trait implemented by all inspect types.
pub trait InspectType: Send + Sync {}

/// Trait implemented by all inspect types. It provides constructor functions that are not
/// intended for use outside the crate.
pub(crate) trait InspectTypeInternal {
    fn new(state: State, block_index: u32) -> Self;
    fn new_no_op() -> Self;
    fn is_valid(&self) -> bool;
}

/// An inner type of all inspect nodes and properties. Each variant implies a
/// different relationship with the underlying inspect VMO.
#[derive(Debug, Derivative)]
#[derivative(Default)]
enum Inner<T: InnerType> {
    /// The node or property is not attached to the inspect VMO.
    #[derivative(Default)]
    None,

    /// The node or property is attached to the inspect VMO, iff its strong
    /// reference is still alive.
    Weak(Weak<InnerRef<T>>),

    /// The node or property is attached to the inspect VMO.
    Strong(Arc<InnerRef<T>>),
}

impl<T: InnerType> Inner<T> {
    /// Creates a new Inner with the desired block index within the inspect VMO
    fn new(state: State, block_index: u32) -> Self {
        Self::Strong(Arc::new(InnerRef { state, block_index, data: T::Data::default() }))
    }

    /// Returns true if the number of strong references to this node or property
    /// is greater than 0.
    fn is_valid(&self) -> bool {
        match self {
            Self::None => false,
            Self::Weak(weak_ref) => weak_ref.strong_count() > 0,
            Self::Strong(_) => true,
        }
    }

    /// Returns a `Some(Arc<InnerRef>)` iff the node or property is currently
    /// attached to inspect, or `None` otherwise. Weak pointers are upgraded
    /// if possible, but their lifetime as strong references are expected to be
    /// short.
    fn inner_ref(&self) -> Option<Arc<InnerRef<T>>> {
        match self {
            Self::None => None,
            Self::Weak(weak_ref) => weak_ref.upgrade(),
            Self::Strong(inner_ref) => Some(Arc::clone(inner_ref)),
        }
    }

    /// Make a weak reference.
    fn clone_weak(&self) -> Self {
        match self {
            Self::None => Self::None,
            Self::Weak(weak_ref) => Self::Weak(weak_ref.clone()),
            Self::Strong(inner_ref) => Self::Weak(Arc::downgrade(inner_ref)),
        }
    }
}

/// Inspect API types implement Eq,PartialEq returning true all the time so that
/// structs embedding inspect types can derive these traits as well.
/// IMPORTANT: Do not rely on these traits implementations for real comparisons
/// or validation tests, instead leverage the reader.
impl<T: InnerType> PartialEq for Inner<T> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<T: InnerType> Eq for Inner<T> {}

/// A type that is owned by inspect nodes and properties, sharing ownership of
/// the inspect VMO heap, and with numerical pointers to the location in the
/// heap in which it resides.
#[derive(Debug)]
struct InnerRef<T: InnerType> {
    /// Index of the block in the VMO.
    block_index: u32,

    /// Reference to the VMO heap.
    state: State,

    /// Associated data for this type.
    data: T::Data,
}

impl<T: InnerType> Drop for InnerRef<T> {
    /// InnerRef has a manual drop impl, to guarantee a single deallocation in
    /// the case of multiple strong references.
    fn drop(&mut self) {
        T::free(&self.state, self.block_index).unwrap();
    }
}

/// De-allocation behavior and associated data for an inner type.
trait InnerType {
    /// Associated data stored on the InnerRef
    type Data: Default + Debug;

    /// De-allocation behavior for when the InnerRef gets dropped
    fn free(state: &State, block_index: u32) -> Result<(), Error>;
}

#[derive(Default, Debug)]
struct InnerNodeType;

impl InnerType for InnerNodeType {
    // Each node has a list of recorded values.
    type Data = ValueList;

    fn free(state: &State, block_index: u32) -> Result<(), Error> {
        if block_index == 0 {
            return Ok(());
        }
        let mut state_lock = state.try_lock()?;
        state_lock.free_value(block_index).map_err(|err| Error::free("node", block_index, err))
    }
}

#[derive(Default, Debug)]
struct InnerValueType;

impl InnerType for InnerValueType {
    type Data = ();
    fn free(state: &State, block_index: u32) -> Result<(), Error> {
        let mut state_lock = state.try_lock()?;
        state_lock.free_value(block_index).map_err(|err| Error::free("value", block_index, err))
    }
}

#[derive(Default, Debug)]
struct InnerPropertyType;

impl InnerType for InnerPropertyType {
    type Data = ();
    fn free(state: &State, block_index: u32) -> Result<(), Error> {
        let mut state_lock = state.try_lock()?;
        state_lock
            .free_property(block_index)
            .map_err(|err| Error::free("property", block_index, err))
    }
}

#[derive(Default, Debug)]
struct InnerLazyNodeType;

impl InnerType for InnerLazyNodeType {
    type Data = ();
    fn free(state: &State, block_index: u32) -> Result<(), Error> {
        let mut state_lock = state.try_lock()?;
        state_lock
            .free_lazy_node(block_index)
            .map_err(|err| Error::free("lazy node", block_index, err))
    }
}

// Utility for generating the implementation of all inspect types (including the struct):
//  - All Inspect Types (*Property, Node) can be No-Op. This macro generates the
//    appropiate internal constructors.
//  - All Inspect Types derive PartialEq, Eq. This generates the dummy implementation
//    for the wrapped type.
macro_rules! inspect_type_impl {
    ($(#[$attr:meta])* struct $name:ident, $type:ident) => {
        paste::paste! {
            $(#[$attr])*
            ///
            /// NOTE: do not rely on PartialEq implementation for true comparison.
            /// Instead leverage the reader.
            ///
            /// NOTE: Operations on a Default value are no-ops.
            #[derive(Debug, PartialEq, Eq, Default)]
            pub struct $name {
                inner: Inner<$type>,
            }

            #[cfg(test)]
            impl $name {
                /// Returns the [`Block`][Block] associated with this value.
                pub fn get_block(&self) -> Option<Block<Arc<Mapping>>> {
                    self.inner.inner_ref().and_then(|inner_ref| {
                        inner_ref.state.try_lock()
                            .and_then(|state| state.heap().get_block(inner_ref.block_index)).ok()
                    })
                }

                /// Returns the index of the value's block in the VMO.
                pub fn block_index(&self) -> u32 {
                    self.inner.inner_ref().unwrap().block_index
                }

            }

            impl InspectType for $name {}

            impl InspectTypeInternal for $name {
                fn new(state: State, block_index: u32) -> Self {
                    Self {
                        inner: Inner::new(state, block_index),
                    }
                }

                fn is_valid(&self) -> bool {
                    self.inner.is_valid()
                }

                fn new_no_op() -> Self {
                    Self { inner: Inner::None }
                }
            }
        }
    }
}

// Utility for generating functions to create a numeric property.
//   `name`: identifier for the name (example: double)
//   `name_cap`: identifier for the name capitalized (example: Double)
//   `type`: the type of the numeric property (example: f64)
macro_rules! create_numeric_property_fn {
    ($name:ident, $name_cap:ident, $type:ident) => {
        paste::paste! {
            #[doc = "Creates a new `" $name_cap "` with the given `name` and `value`."]
            #[must_use]
            pub fn [<create_ $name >](&self, name: impl AsRef<str>, value: $type)
                -> [<$name_cap Property>] {
                    self.inner.inner_ref().and_then(|inner_ref| {
                        inner_ref.state
                            .try_lock()
                            .and_then(|mut state| {
                                state.[<create_ $name _metric>](
                                    name.as_ref(), value, inner_ref.block_index)
                            })
                            .map(|block| {
                                [<$name_cap Property>]::new(inner_ref.state.clone(), block.index())
                            })
                            .ok()
                    })
                    .unwrap_or([<$name_cap Property>]::new_no_op())
            }

            #[doc = "Records a new `" $name_cap "` with the given `name` and `value`."]
            pub fn [<record_ $name >](&self, name: impl AsRef<str>, value: $type) {
                let property = self.[<create_ $name>](name, value);
                self.record(property);
            }
        }
    };
}

// Utility for generating functions to create an array property.
//   `name`: identifier for the name (example: double)
//   `name_cap`: identifier for the name capitalized (example: Double)
//   `type`: the type of the numeric property (example: f64)
macro_rules! create_array_property_fn {
    ($name:ident, $name_cap:ident, $type:ident) => {
        paste::paste! {
            #[doc = "Creates a new `" $name_cap "ArrayProperty` with the given `name` and `slots`."]
            #[must_use]
            pub fn [<create_ $name _array>](&self, name: impl AsRef<str>, slots: usize)
                -> [<$name_cap ArrayProperty>] {
                    self.[<create_ $name _array_internal>](name, slots, ArrayFormat::Default)
            }

            fn [<create_ $name _array_internal>](
                &self, name: impl AsRef<str>, slots: usize, format: ArrayFormat)
                -> [<$name_cap ArrayProperty>] {
                    self.inner.inner_ref().and_then(|inner_ref| {
                        inner_ref.state
                            .try_lock()
                            .and_then(|mut state| {
                                state.[<create_ $name _array>](
                                    name.as_ref(), slots, format, inner_ref.block_index)
                            })
                            .map(|block| {
                                [<$name_cap ArrayProperty>]::new(inner_ref.state.clone(), block.index())
                            })
                            .ok()
                    })
                    .unwrap_or([<$name_cap ArrayProperty>]::new_no_op())
            }
        }
    };
}

// Utility for generating functions to create a linear histogram property.
//   `name`: identifier for the name (example: double)
//   `name_cap`: identifier for the name capitalized (example: Double)
//   `type`: the type of the numeric property (example: f64)
macro_rules! create_linear_histogram_property_fn {
    ($name:ident, $name_cap:ident, $type:ident) => {
        paste::paste! {
            #[doc = "Creates a new `" $name_cap
               "LinearHistogramProperty` with the given `name` and `params`."]
            #[must_use]
            pub fn [<create_ $name _linear_histogram>](
                &self, name: impl AsRef<str>, params: LinearHistogramParams<$type>)
                -> [<$name_cap LinearHistogramProperty>] {
                let slots = params.buckets + constants::LINEAR_HISTOGRAM_EXTRA_SLOTS;
                let array = self.[<create_ $name _array_internal>](
                    name, slots, ArrayFormat::LinearHistogram);
                array.set(0, params.floor);
                array.set(1, params.step_size);
                [<$name_cap LinearHistogramProperty>] {
                    floor: params.floor,
                    step_size: params.step_size,
                    slots,
                    array
                }
            }
        }
    };
}

// Utility for generating functions to create an exponential histogram property.
//   `name`: identifier for the name (example: double)
//   `name_cap`: identifier for the name capitalized (example: Double)
//   `type`: the type of the numeric property (example: f64)
macro_rules! create_exponential_histogram_property_fn {
    ($name:ident, $name_cap:ident, $type:ident) => {
        paste::paste! {
            #[doc = "Creates a new `" $name_cap
               "ExponentialHistogramProperty` with the given `name` and `params`."]
            #[must_use]
            pub fn [<create_ $name _exponential_histogram>](
              &self, name: impl AsRef<str>, params: ExponentialHistogramParams<$type>)
              -> [<$name_cap ExponentialHistogramProperty>] {
                let slots = params.buckets + constants::EXPONENTIAL_HISTOGRAM_EXTRA_SLOTS;
                let array = self.[<create_ $name _array_internal>](
                    name, slots, ArrayFormat::ExponentialHistogram);
                array.set(0, params.floor);
                array.set(1, params.initial_step);
                array.set(2, params.step_multiplier);
                [<$name_cap ExponentialHistogramProperty>] {
                    floor: params.floor,
                    initial_step: params.initial_step,
                    step_multiplier: params.step_multiplier,
                    slots,
                    array
                }
            }
        }
    };
}

/// Utility for generating functions to create lazy nodes.
///   `fn_suffix`: identifier for the fn name.
///   `disposition`: identifier for the type of LinkNodeDisposition.
macro_rules! create_lazy_property_fn {
    ($fn_suffix:ident, $disposition:ident) => {
        paste::paste! {
            #[must_use]
            #[doc = "Creates a new lazy " $fn_suffix " link with the given `name` and `callback`."]
            pub fn [<create_lazy_ $fn_suffix>]<F>(&self, name: impl AsRef<str>, callback: F) -> LazyNode
            where F: Fn() -> BoxFuture<'static, Result<Inspector, anyhow::Error>> + Sync + Send + 'static {
                self.inner.inner_ref().and_then(|inner_ref| {
                    inner_ref
                        .state
                        .try_lock()
                        .and_then(|mut state| state.create_lazy_node(
                            name.as_ref(),
                            inner_ref.block_index,
                            LinkNodeDisposition::$disposition,
                            callback,
                        ))
                        .map(|block| LazyNode::new(inner_ref.state.clone(), block.index()))
                        .ok()

                })
                .unwrap_or(LazyNode::new_no_op())
            }

            #[doc = "Records a new lazy " $fn_suffix " link with the given `name` and `callback`."]
            pub fn [<record_lazy_ $fn_suffix>]<F>(
                &self, name: impl AsRef<str>, callback: F)
            where F: Fn() -> BoxFuture<'static, Result<Inspector, anyhow::Error>> + Sync + Send + 'static {
                let property = self.[<create_lazy_ $fn_suffix>](name, callback);
                self.record(property);
            }
        }
    }
}

inspect_type_impl!(
    /// Inspect Node data type.
    struct Node,
    InnerNodeType
);

inspect_type_impl!(
    /// Inspect Lazy Node data type.
    struct LazyNode,
    InnerLazyNodeType
);

impl Node {
    /// Creates a new root node.
    pub(in crate) fn new_root(state: State) -> Node {
        Node::new(state, 0)
    }

    /// Returns the inner state where operations in this node write.
    pub(in crate) fn state(&self) -> Option<State> {
        self.inner.inner_ref().map(|inner_ref| inner_ref.state.clone())
    }

    /// Create a weak reference to the original node. All operations on a weak
    /// reference have identical semantics to the original node for as long
    /// as the original node is live. After that, all operations are no-ops.
    pub fn clone_weak(&self) -> Node {
        Self { inner: self.inner.clone_weak() }
    }

    /// Creates and keeps track of a child with the given `name`.
    pub fn record_child<F>(&self, name: impl AsRef<str>, initialize: F)
    where
        F: FnOnce(&Node),
    {
        let child = self.create_child(name);
        initialize(&child);
        self.record(child);
    }

    /// Add a child to this node.
    #[must_use]
    pub fn create_child(&self, name: impl AsRef<str>) -> Node {
        self.inner
            .inner_ref()
            .and_then(|inner_ref| {
                inner_ref
                    .state
                    .try_lock()
                    .and_then(|mut state| state.create_node(name.as_ref(), inner_ref.block_index))
                    .map(|block| Node::new(inner_ref.state.clone(), block.index()))
                    .ok()
            })
            .unwrap_or(Node::new_no_op())
    }

    /// Takes a function to execute as under a single lock of the Inspect VMO. This function
    /// receives a reference to the `Node` where this is called.
    pub fn atomic_update<F, R>(&self, mut update_fn: F) -> R
    where
        F: FnMut(&Node) -> R,
    {
        match self.inner.inner_ref() {
            None => {
                // If the node was a no-op we still execute the `update_fn` even if all operations
                // inside it will be no-ops to return `R`.
                update_fn(&self)
            }
            Some(inner_ref) => {
                // Silently ignore the error when fail to lock (as in any regular operation).
                // All operations performed in the `update_fn` won't update the vmo
                // generation count since we'll be holding one lock here.
                inner_ref.state.begin_transaction();
                let result = update_fn(&self);
                inner_ref.state.end_transaction();
                result
            }
        }
    }

    /// Keeps track of the given property for the lifetime of the node.
    pub fn record(&self, property: impl InspectType + 'static) {
        self.inner.inner_ref().map(|inner_ref| inner_ref.data.record(property));
    }

    // Add a lazy node property to this node:
    // - create_lazy_node: adds a lazy child to this node. This node will be
    //   populated by the given callback on demand.
    // - create_lazy_values: adds a lazy child to this node. The lazy node
    //   children and properties are added to this node on demand. Name is only
    //   used in the event that a reader does not obtain the values.
    create_lazy_property_fn!(child, Child);
    create_lazy_property_fn!(values, Inline);

    // Add a numeric property to this node: create_int, create_double,
    // create_uint.
    create_numeric_property_fn!(int, Int, i64);
    create_numeric_property_fn!(uint, Uint, u64);
    create_numeric_property_fn!(double, Double, f64);

    // Add an array property to this node: create_int_array, create_double_array,
    // create_uint_array.
    create_array_property_fn!(int, Int, i64);
    create_array_property_fn!(uint, Uint, u64);
    create_array_property_fn!(double, Double, f64);

    // Add a linear histogram property to this node: create_int_linear_histogram,
    // create_uint_linear_histogram, create_double_linear_histogram.
    create_linear_histogram_property_fn!(int, Int, i64);
    create_linear_histogram_property_fn!(uint, Uint, u64);
    create_linear_histogram_property_fn!(double, Double, f64);

    // Add an exponential histogram property to this node: create_int_exponential_histogram,
    // create_uint_exponential_histogram, create_double_exponential_histogram.
    create_exponential_histogram_property_fn!(int, Int, i64);
    create_exponential_histogram_property_fn!(uint, Uint, u64);
    create_exponential_histogram_property_fn!(double, Double, f64);

    /// Creates a lazy node from the given VMO.
    #[must_use]
    pub fn create_lazy_child_from_vmo(&self, name: impl AsRef<str>, vmo: Arc<zx::Vmo>) -> LazyNode {
        self.create_lazy_child(name.as_ref(), move || {
            let vmo_clone = vmo.clone();
            async move { Ok(Inspector::no_op_from_vmo(vmo_clone)) }.boxed()
        })
    }

    /// Records a lazy node from the given VMO.
    pub fn record_lazy_child_from_vmo(&self, name: impl AsRef<str>, vmo: Arc<zx::Vmo>) {
        self.record_lazy_child(name.as_ref(), move || {
            let vmo_clone = vmo.clone();
            async move { Ok(Inspector::no_op_from_vmo(vmo_clone)) }.boxed()
        });
    }

    /// Add a string property to this node.
    #[must_use]
    pub fn create_string(&self, name: impl AsRef<str>, value: impl AsRef<str>) -> StringProperty {
        self.inner
            .inner_ref()
            .and_then(|inner_ref| {
                inner_ref
                    .state
                    .try_lock()
                    .and_then(|mut state| {
                        state.create_property(
                            name.as_ref(),
                            value.as_ref().as_bytes(),
                            PropertyFormat::String,
                            inner_ref.block_index,
                        )
                    })
                    .map(|block| StringProperty::new(inner_ref.state.clone(), block.index()))
                    .ok()
            })
            .unwrap_or(StringProperty::new_no_op())
    }

    /// Creates and saves a string property for the lifetime of the node.
    pub fn record_string(&self, name: impl AsRef<str>, value: impl AsRef<str>) {
        let property = self.create_string(name, value);
        self.record(property);
    }

    /// Add a byte vector property to this node.
    #[must_use]
    pub fn create_bytes(&self, name: impl AsRef<str>, value: impl AsRef<[u8]>) -> BytesProperty {
        self.inner
            .inner_ref()
            .and_then(|inner_ref| {
                inner_ref
                    .state
                    .try_lock()
                    .and_then(|mut state| {
                        state.create_property(
                            name.as_ref(),
                            value.as_ref(),
                            PropertyFormat::Bytes,
                            inner_ref.block_index,
                        )
                    })
                    .map(|block| BytesProperty::new(inner_ref.state.clone(), block.index()))
                    .ok()
            })
            .unwrap_or(BytesProperty::new_no_op())
    }

    /// Creates and saves a bytes property for the lifetime of the node.
    pub fn record_bytes(&self, name: impl AsRef<str>, value: impl AsRef<[u8]>) {
        let property = self.create_bytes(name, value);
        self.record(property);
    }

    /// Add a bool property to this node.
    #[must_use]
    pub fn create_bool(&self, name: impl AsRef<str>, value: bool) -> BoolProperty {
        self.inner
            .inner_ref()
            .and_then(|inner_ref| {
                inner_ref
                    .state
                    .try_lock()
                    .and_then(|mut state| {
                        state.create_bool(name.as_ref(), value, inner_ref.block_index)
                    })
                    .map(|block| BoolProperty::new(inner_ref.state.clone(), block.index()))
                    .ok()
            })
            .unwrap_or(BoolProperty::new_no_op())
    }

    /// Creates and saves a bool property for the lifetime of the node.
    pub fn record_bool(&self, name: impl AsRef<str>, value: bool) {
        let property = self.create_bool(name, value);
        self.record(property);
    }
}

/// Trait implemented by properties.
pub trait Property<'t> {
    /// The type of the property.
    type Type;

    /// Set the property value to |value|.
    fn set(&'t self, value: Self::Type);
}

/// Trait implemented by numeric properties providing common operations.
pub trait NumericProperty {
    /// The type the property is handling.
    type Type;

    /// Add the given |value| to the property current value.
    fn add(&self, value: Self::Type);

    /// Subtract the given |value| from the property current value.
    fn subtract(&self, value: Self::Type);

    /// Return the current value of the property for testing.
    /// NOTE: This is a temporary feature to aid unit test of Inspect clients.
    /// It will be replaced by a more comprehensive Read API implementation.
    fn get(&self) -> Result<Self::Type, Error>;
}

// Utility for generating numeric property functions (example: set, add, subtract)
//   `fn_name`: the name of the function to generate (example: set)
//   `type`: the type of the argument of the function to generate (example: f64)
//   `name`: the readble name of the type of the function (example: double)
macro_rules! numeric_property_fn {
    ($fn_name:ident, $type:ident, $name:ident) => {
        paste::paste! {
            // Docs here come from the trait docs.
            fn $fn_name(&self, value: $type) {
                if let Some(ref inner_ref) = self.inner.inner_ref() {
                    inner_ref.state.try_lock()
                        .and_then(|state| {
                            state.[<$fn_name _ $name _metric>](inner_ref.block_index, value)
                        })
                        .unwrap_or_else(|e| {
                            error!("Failed to {} property. Error: {:?}", stringify!($fn_name), e);
                        });
                }
            }
        }
    };
}

// Utility for generating a numeric property datatype impl
//   `name`: the readble name of the type of the function (example: double)
//   `name_cap`: the capitalized readble name of the type of the function (example: Double)
//   `type`: the type of the argument of the function to generate (example: f64)
macro_rules! numeric_property {
    ($name:ident, $name_cap:ident, $type:ident) => {
        paste::paste! {
            inspect_type_impl!(
                /// Inspect API Numeric Property data type.
                struct [<$name_cap Property>],
                InnerValueType
            );

            impl<'t> Property<'t> for [<$name_cap Property>] {
                type Type = $type;

                numeric_property_fn!(set, $type, $name);
            }

            impl NumericProperty for [<$name_cap Property>] {
                type Type = $type;
                numeric_property_fn!(add, $type, $name);
                numeric_property_fn!(subtract, $type, $name);

                fn get(&self) -> Result<$type, Error> {
                    if let Some(ref inner_ref) = self.inner.inner_ref() {
                        inner_ref.state
                            .try_lock()
                            .and_then(|state| state.[<get_ $name _metric>](inner_ref.block_index))
                    } else {
                        Err(Error::NoOp("Property"))
                    }
                }
            }
        }
    };
}

numeric_property!(int, Int, i64);
numeric_property!(uint, Uint, u64);
numeric_property!(double, Double, f64);

// Utility for generating a byte/string property datatype impl
//   `name`: the readable name of the type of the function (example: String)
//   `type`: the type of the argument of the function to generate (example: str)
//   `bytes`: an optional method to get the bytes of the property
macro_rules! property {
    ($name:ident, $type:ty $(, $bytes:ident)?) => {
        paste::paste! {
            inspect_type_impl!(
                /// Inspect API Property data type.
                struct [<$name Property>],
                InnerPropertyType
            );

            impl<'t> Property<'t> for [<$name Property>] {
                type Type = &'t $type;

                fn set(&'t self, value: &'t $type) {
                    if let Some(ref inner_ref) = self.inner.inner_ref() {
                        inner_ref.state
                            .try_lock()
                            .and_then(|mut state| {
                                state.set_property(inner_ref.block_index, value$(.$bytes())?)
                            })
                            .unwrap_or_else(|e| error!("Failed to set property. Error: {:?}", e));
                    }
                }

            }
        }
    };
}

property!(String, str, as_bytes);
property!(Bytes, [u8]);

inspect_type_impl!(
    /// Inspect API Bool Property data type.
    struct BoolProperty,
    InnerValueType
);

impl<'t> Property<'t> for BoolProperty {
    type Type = bool;

    fn set(&self, value: bool) {
        if let Some(ref inner_ref) = self.inner.inner_ref() {
            inner_ref
                .state
                .try_lock()
                .and_then(|state| state.set_bool(inner_ref.block_index, value))
                .unwrap_or_else(|e| {
                    error!("Failed to set property. Error: {:?}", e);
                });
        }
    }
}

/// Trait implemented by all array properties providing common operations on arrays.
pub trait ArrayProperty {
    /// The type of the array entries.
    type Type;

    /// Sets the array value to `value` at the given `index`.
    fn set(&self, index: usize, value: Self::Type);

    /// Adds the given `value` to the property current value at the given `index`.
    fn add(&self, index: usize, value: Self::Type);

    /// Subtracts the given `value` to the property current value at the given `index`.
    fn subtract(&self, index: usize, value: Self::Type);

    /// Sets all slots of the array to 0.
    fn clear(&self);
}

// Utility for generating array property functions (example: set, add, subtract)
//   `fn_name`: the name of the function to generate (example: set)
//   `type`: the type of the argument of the function to generate (example: f64)
//   `name`: the readble name of the type of the function (example: double)
macro_rules! array_property_fn {
    ($fn_name:ident, $type:ident, $name:ident) => {
        paste::paste! {
            // Docs here come from the trait docs.
            fn $fn_name(&self, index: usize, value: $type) {
                if let Some(ref inner_ref) = self.inner.inner_ref() {
                    inner_ref.state
                        .try_lock()
                        .and_then(|mut state| {
                            state.[<$fn_name _array_ $name _slot>](
                                inner_ref.block_index, index, value)
                        })
                        .unwrap_or_else(|e| {
                            error!("Failed to {} property. Error: {:?}", stringify!($fn_name), e);
                        });
                }
            }
        }
    };
}

// Utility for generating a numeric array datatype impl
//   `name`: the readble name of the type of the function (example: double)
//   `type`: the type of the argument of the function to generate (example: f64)
macro_rules! array_property {
    ($name:ident, $name_cap:ident, $type:ident) => {
        paste::paste! {
            inspect_type_impl!(
                /// Inspect API Array Property data type.
                struct [<$name_cap ArrayProperty>],
                InnerValueType
            );

            impl [<$name_cap ArrayProperty>] {
            }

            impl ArrayProperty for [<$name_cap ArrayProperty>] {
                type Type = $type;

                array_property_fn!(set, $type, $name);
                array_property_fn!(add, $type, $name);
                array_property_fn!(subtract, $type, $name);

                fn clear(&self) {
                    if let Some(ref inner_ref) = self.inner.inner_ref() {
                        inner_ref.state
                            .try_lock()
                            .and_then(|mut state| state.clear_array(inner_ref.block_index, 0))
                            .unwrap_or_else(|e| {
                                error!("Failed to clear property. Error: {:?}", e);
                            });
                    }
                }
            }
        }
    };
}

array_property!(int, Int, i64);
array_property!(uint, Uint, u64);
array_property!(double, Double, f64);

/// Trait implemented by all hitogram properties providing common operations.
pub trait HistogramProperty {
    /// The type of each value added to the histogram.
    type Type;

    /// Inserts the given `value` in the histogram.
    fn insert(&self, value: Self::Type);

    /// Inserts the given `value` in the histogram `count` times.
    fn insert_multiple(&self, value: Self::Type, count: usize);

    /// Clears all buckets of the histogram.
    fn clear(&self);
}

macro_rules! histogram_property {
    ($histogram_type:ident, $name_cap:ident, $type:ident, $clear_start_index:expr) => {
        paste::paste! {
            impl HistogramProperty for [<$name_cap $histogram_type HistogramProperty>] {
                type Type = $type;

                fn insert(&self, value: $type) {
                    self.insert_multiple(value, 1);
                }

                fn insert_multiple(&self, value: $type, count: usize) {
                    self.array.add(self.get_index(value), count as $type);
                }

                fn clear(&self) {
                    if let Some(ref inner_ref) = self.array.inner.inner_ref() {
                        // Ensure we don't delete the array slots that contain histogram metadata.
                        inner_ref.state
                            .try_lock()
                            .and_then(|mut state| {
                                state.clear_array(inner_ref.block_index, $clear_start_index)
                            })
                            .unwrap_or_else(|e| {
                                error!("Failed to {} property. Error: {:?}", stringify!($fn_name), e);
                            });
                    }
                }
            }
        }
    };
}

macro_rules! linear_histogram_property {
    ($name_cap:ident, $type:ident) => {
        paste::paste! {
            #[derive(Debug)]
            #[doc = "A linear histogram property for " $type " values."]
            pub struct [<$name_cap LinearHistogramProperty>] {
                array: [<$name_cap ArrayProperty>],
                floor: $type,
                slots: usize,
                step_size: $type,
            }

            impl [<$name_cap LinearHistogramProperty>] {
                fn get_index(&self, value: $type) -> usize {
                    let mut current_floor = self.floor;
                    // Start in the underflow index.
                    let mut index = constants::LINEAR_HISTOGRAM_EXTRA_SLOTS - 2;
                    while value >= current_floor && index < self.slots - 1 {
                        current_floor += self.step_size;
                        index += 1;
                    }
                    index as usize
                }

                #[cfg(test)]
                fn get_block(&self) -> Option<Block<Arc<Mapping>>> {
                    self.array.get_block()
                }
            }

            histogram_property!(
                Linear, $name_cap, $type,
                // -2 = the overflow and underflow slots which still need to be cleared.
                constants::LINEAR_HISTOGRAM_EXTRA_SLOTS - 2);
        }
    };
}

macro_rules! exponential_histogram_property {
    ($name_cap:ident, $type:ident) => {
        paste::paste! {
            #[derive(Debug)]
            #[doc = "An exponential histogram property for " $type " values."]
            pub struct [<$name_cap ExponentialHistogramProperty>] {
                array: [<$name_cap ArrayProperty>],
                floor: $type,
                initial_step: $type,
                step_multiplier: $type,
                slots: usize,
            }

            impl [<$name_cap ExponentialHistogramProperty>] {
                fn get_index(&self, value: $type) -> usize {
                    let mut current_floor = self.floor;
                    let mut offset = self.initial_step;
                    // Start in the underflow index.
                    let mut index = constants::EXPONENTIAL_HISTOGRAM_EXTRA_SLOTS - 2;
                    while value >= current_floor && index < self.slots - 1 {
                        current_floor = self.floor + offset;
                        offset *= self.step_multiplier;
                        index += 1;
                    }
                    index as usize
                }

                #[cfg(test)]
                fn get_block(&self) -> Option<Block<Arc<Mapping>>> {
                    self.array.get_block()
                }
            }

            histogram_property!(
                Exponential, $name_cap, $type,
                // -2 = the overflow and underflow slots which still need to be cleared.
                constants::EXPONENTIAL_HISTOGRAM_EXTRA_SLOTS - 2);
        }
    };
}

linear_histogram_property!(Double, f64);
linear_histogram_property!(Int, i64);
linear_histogram_property!(Uint, u64);
exponential_histogram_property!(Double, f64);
exponential_histogram_property!(Int, i64);
exponential_histogram_property!(Uint, u64);

/// Generates a unique name that can be used in inspect nodes and properties that will be prefixed
/// by the given `prefix`.
pub fn unique_name(prefix: &str) -> String {
    let suffix = UNIQUE_NAME_SUFFIX.fetch_add(1, Ordering::Relaxed);
    format!("{}{}", prefix, suffix)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{assert_data_tree, heap::Heap, reader},
        diagnostics_hierarchy::DiagnosticsHierarchy,
        fuchsia_zircon::{AsHandleRef, Peered},
        inspect_format::{constants, BlockType, LinkNodeDisposition},
        mapped_vmo::Mapping,
        std::{convert::TryFrom, ffi::CString},
    };

    #[test]
    fn inspector_new() {
        let test_object = Inspector::new();
        assert_eq!(
            test_object.vmo.as_ref().unwrap().get_size().unwrap(),
            constants::DEFAULT_VMO_SIZE_BYTES as u64
        );
    }

    #[test]
    fn inspector_duplicate_vmo() {
        let test_object = Inspector::new();
        assert_eq!(
            test_object.vmo.as_ref().unwrap().get_size().unwrap(),
            constants::DEFAULT_VMO_SIZE_BYTES as u64
        );
        assert_eq!(
            test_object.duplicate_vmo().unwrap().get_size().unwrap(),
            constants::DEFAULT_VMO_SIZE_BYTES as u64
        );
    }

    #[test]
    fn inspector_copy_data() {
        let test_object = Inspector::new();

        assert_eq!(
            test_object.vmo.as_ref().unwrap().get_size().unwrap(),
            constants::DEFAULT_VMO_SIZE_BYTES as u64
        );
        // The copy will be a single page, since that is all that is used.
        assert_eq!(test_object.copy_vmo_data().unwrap().len(), 4096);
    }

    #[test]
    fn no_op() {
        let inspector = Inspector::new_with_size(4096);
        // Make the VMO full.
        let nodes = (0..127).map(|_| inspector.root().create_child("test")).collect::<Vec<Node>>();

        assert!(nodes.iter().all(|node| node.is_valid()));
        let no_op_node = inspector.root().create_child("no-op-child");
        assert!(!no_op_node.is_valid());
    }

    #[test]
    fn inspector_new_with_size() {
        let test_object = Inspector::new_with_size(8192);
        assert_eq!(test_object.vmo.as_ref().unwrap().get_size().unwrap(), 8192);
        assert_eq!(
            CString::new("InspectHeap").unwrap(),
            test_object.vmo.as_ref().unwrap().get_name().expect("Has name")
        );

        // If size is not a multiple of 4096, it'll be rounded up.
        let test_object = Inspector::new_with_size(10000);
        assert_eq!(test_object.vmo.unwrap().get_size().unwrap(), 12288);

        // If size is less than the minimum size, the minimum will be set.
        let test_object = Inspector::new_with_size(2000);
        assert_eq!(test_object.vmo.unwrap().get_size().unwrap(), 4096);
    }

    #[test]
    fn inspector_new_root() {
        // Note, the small size we request should be rounded up to a full 4kB page.
        let (vmo, root_node) = Inspector::new_root(100).unwrap();
        assert_eq!(vmo.get_size().unwrap(), 4096);
        let inner = root_node.inner.inner_ref().unwrap();
        assert_eq!(inner.block_index, 0);
        assert_eq!(CString::new("InspectHeap").unwrap(), vmo.get_name().expect("Has name"));
    }

    #[test]
    fn node() {
        // Create and use a default value.
        let default = Node::default();
        default.record_int("a", 0);

        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        assert_eq!(node_block.block_type(), BlockType::NodeValue);
        assert_eq!(node_block.child_count().unwrap(), 0);
        {
            let child = node.create_child("child");
            let child_block = child.get_block().unwrap();
            assert_eq!(child_block.block_type(), BlockType::NodeValue);
            assert_eq!(child_block.child_count().unwrap(), 0);
            assert_eq!(node_block.child_count().unwrap(), 1);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn node_no_op_clone_weak() {
        let default = Node::default();
        assert!(!default.is_valid());
        let weak = default.clone_weak();
        assert!(!weak.is_valid());
        let _ = weak.create_child("child");
        std::mem::drop(default);
        let _ = weak.create_uint("age", 1337);
        assert!(!weak.is_valid());
    }

    #[test]
    fn node_clone_weak() {
        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_weak = node.clone_weak();
        let node_weak_2 = node_weak.clone_weak(); // Weak from another weak

        let node_block = node.get_block().unwrap();
        assert_eq!(node_block.block_type(), BlockType::NodeValue);
        assert_eq!(node_block.child_count().unwrap(), 0);
        let node_weak_block = node.get_block().unwrap();
        assert_eq!(node_weak_block.block_type(), BlockType::NodeValue);
        assert_eq!(node_weak_block.child_count().unwrap(), 0);
        let node_weak_2_block = node.get_block().unwrap();
        assert_eq!(node_weak_2_block.block_type(), BlockType::NodeValue);
        assert_eq!(node_weak_2_block.child_count().unwrap(), 0);

        let child_from_strong = node.create_child("child");
        let child = node_weak.create_child("child_1");
        let child_2 = node_weak_2.create_child("child_2");
        std::mem::drop(node_weak_2);
        assert_eq!(node_weak_block.child_count().unwrap(), 3);
        std::mem::drop(child_from_strong);
        assert_eq!(node_weak_block.child_count().unwrap(), 2);
        std::mem::drop(child);
        assert_eq!(node_weak_block.child_count().unwrap(), 1);
        assert!(node_weak.is_valid());
        assert!(child_2.is_valid());
        std::mem::drop(node);
        assert!(!node_weak.is_valid());
        let _ = node_weak.create_child("orphan");
        let _ = child_2.create_child("orphan");
    }

    #[test]
    fn double_property() {
        // Create and use a default value.
        let default = DoubleProperty::default();
        default.add(1.0);

        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let property = node.create_double("property", 1.0);
            let property_block = property.get_block().unwrap();
            assert_eq!(property_block.block_type(), BlockType::DoubleValue);
            assert_eq!(property_block.double_value().unwrap(), 1.0);
            assert_eq!(node_block.child_count().unwrap(), 1);

            property.set(2.0);
            assert_eq!(property_block.double_value().unwrap(), 2.0);
            assert_eq!(property.get().unwrap(), 2.0);

            property.subtract(5.5);
            assert_eq!(property_block.double_value().unwrap(), -3.5);

            property.add(8.1);
            assert_eq!(property_block.double_value().unwrap(), 4.6);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn int_property() {
        // Create and use a default value.
        let default = IntProperty::default();
        default.add(1);

        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let property = node.create_int("property", 1);
            let property_block = property.get_block().unwrap();
            assert_eq!(property_block.block_type(), BlockType::IntValue);
            assert_eq!(property_block.int_value().unwrap(), 1);
            assert_eq!(node_block.child_count().unwrap(), 1);

            property.set(2);
            assert_eq!(property_block.int_value().unwrap(), 2);
            assert_eq!(property.get().unwrap(), 2);

            property.subtract(5);
            assert_eq!(property_block.int_value().unwrap(), -3);

            property.add(8);
            assert_eq!(property_block.int_value().unwrap(), 5);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn uint_property() {
        // Create and use a default value.
        let default = UintProperty::default();
        default.add(1);

        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let property = node.create_uint("property", 1);
            let property_block = property.get_block().unwrap();
            assert_eq!(property_block.block_type(), BlockType::UintValue);
            assert_eq!(property_block.uint_value().unwrap(), 1);
            assert_eq!(node_block.child_count().unwrap(), 1);

            property.set(5);
            assert_eq!(property_block.uint_value().unwrap(), 5);
            assert_eq!(property.get().unwrap(), 5);

            property.subtract(3);
            assert_eq!(property_block.uint_value().unwrap(), 2);

            property.add(8);
            assert_eq!(property_block.uint_value().unwrap(), 10);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn bool_property() {
        // Create and use a default value.
        let default = BoolProperty::default();
        default.set(true);

        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let property = node.create_bool("property", true);
            let property_block = property.get_block().unwrap();
            assert_eq!(property_block.block_type(), BlockType::BoolValue);
            assert_eq!(property_block.bool_value().unwrap(), true);
            assert_eq!(node_block.child_count().unwrap(), 1);

            property.set(false);
            assert_eq!(property_block.bool_value().unwrap(), false);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn string_property() {
        // Create and use a default value.
        let default = StringProperty::default();
        default.set("test");

        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let property = node.create_string("property", "test");
            let property_block = property.get_block().unwrap();
            assert_eq!(property_block.block_type(), BlockType::BufferValue);
            assert_eq!(property_block.property_total_length().unwrap(), 4);
            assert_eq!(property_block.property_format().unwrap(), PropertyFormat::String);
            assert_eq!(node_block.child_count().unwrap(), 1);

            property.set("test-set");
            assert_eq!(property_block.property_total_length().unwrap(), 8);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn bytes_property() {
        // Create and use a default value.
        let default = BytesProperty::default();
        default.set(&[0u8, 3u8]);

        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let property = node.create_bytes("property", b"test");
            let property_block = property.get_block().unwrap();
            assert_eq!(property_block.block_type(), BlockType::BufferValue);
            assert_eq!(property_block.property_total_length().unwrap(), 4);
            assert_eq!(property_block.property_format().unwrap(), PropertyFormat::Bytes);
            assert_eq!(node_block.child_count().unwrap(), 1);

            property.set(b"test-set");
            assert_eq!(property_block.property_total_length().unwrap(), 8);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn test_array() {
        // Create and use a default value.
        let default = DoubleArrayProperty::default();
        default.add(1, 1.0);
        let default = IntArrayProperty::default();
        default.add(1, 1);
        let default = UintArrayProperty::default();
        default.add(1, 1);

        let inspector = Inspector::new();
        let root = inspector.root();
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let array = node.create_double_array("array_property", 5);
            let array_block = array.get_block().unwrap();

            array.set(0, 5.0);
            assert_eq!(array_block.array_get_double_slot(0).unwrap(), 5.0);

            array.add(0, 5.3);
            assert_eq!(array_block.array_get_double_slot(0).unwrap(), 10.3);

            array.subtract(0, 3.4);
            assert_eq!(array_block.array_get_double_slot(0).unwrap(), 6.9);

            array.set(1, 2.5);
            array.set(3, -3.1);

            for (i, value) in [6.9, 2.5, 0.0, -3.1, 0.0].iter().enumerate() {
                assert_eq!(array_block.array_get_double_slot(i).unwrap(), *value);
            }

            array.clear();
            for i in 0..5 {
                assert_eq!(0.0, array_block.array_get_double_slot(i).unwrap());
            }

            assert_eq!(node_block.child_count().unwrap(), 1);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn linear_histograms() {
        let inspector = Inspector::new();
        let root = inspector.root();
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let int_histogram = node.create_int_linear_histogram(
                "int-histogram",
                LinearHistogramParams { floor: 10, step_size: 5, buckets: 5 },
            );
            int_histogram.insert_multiple(-1, 2); // underflow
            int_histogram.insert(25);
            int_histogram.insert(500); // overflow
            let block = int_histogram.get_block().unwrap();
            for (i, value) in [10, 5, 2, 0, 0, 0, 1, 0, 1].iter().enumerate() {
                assert_eq!(block.array_get_int_slot(i).unwrap(), *value);
            }

            let uint_histogram = node.create_uint_linear_histogram(
                "uint-histogram",
                LinearHistogramParams { floor: 10, step_size: 5, buckets: 5 },
            );
            uint_histogram.insert_multiple(0, 2); // underflow
            uint_histogram.insert(25);
            uint_histogram.insert(500); // overflow
            let block = uint_histogram.get_block().unwrap();
            for (i, value) in [10, 5, 2, 0, 0, 0, 1, 0, 1].iter().enumerate() {
                assert_eq!(block.array_get_uint_slot(i).unwrap(), *value);
            }

            uint_histogram.clear();
            for (i, value) in [10, 5, 0, 0, 0, 0, 0, 0, 0].iter().enumerate() {
                assert_eq!(*value, block.array_get_uint_slot(i).unwrap());
            }

            let double_histogram = node.create_double_linear_histogram(
                "double-histogram",
                LinearHistogramParams { floor: 10.0, step_size: 5.0, buckets: 5 },
            );
            double_histogram.insert_multiple(0.0, 2); // underflow
            double_histogram.insert(25.3);
            double_histogram.insert(500.0); // overflow
            let block = double_histogram.get_block().unwrap();
            for (i, value) in [10.0, 5.0, 2.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0].iter().enumerate() {
                assert_eq!(block.array_get_double_slot(i).unwrap(), *value);
            }

            assert_eq!(node_block.child_count().unwrap(), 3);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn exponential_histograms() {
        let inspector = Inspector::new();
        let root = inspector.root();
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let int_histogram = node.create_int_exponential_histogram(
                "int-histogram",
                ExponentialHistogramParams {
                    floor: 1,
                    initial_step: 1,
                    step_multiplier: 2,
                    buckets: 4,
                },
            );
            int_histogram.insert_multiple(-1, 2); // underflow
            int_histogram.insert(8);
            int_histogram.insert(500); // overflow
            let block = int_histogram.get_block().unwrap();
            for (i, value) in [1, 1, 2, 2, 0, 0, 0, 1, 1].iter().enumerate() {
                assert_eq!(block.array_get_int_slot(i).unwrap(), *value);
            }

            let uint_histogram = node.create_uint_exponential_histogram(
                "uint-histogram",
                ExponentialHistogramParams {
                    floor: 1,
                    initial_step: 1,
                    step_multiplier: 2,
                    buckets: 4,
                },
            );
            uint_histogram.insert_multiple(0, 2); // underflow
            uint_histogram.insert(8);
            uint_histogram.insert(500); // overflow
            let block = uint_histogram.get_block().unwrap();
            for (i, value) in [1, 1, 2, 2, 0, 0, 0, 1, 1].iter().enumerate() {
                assert_eq!(block.array_get_uint_slot(i).unwrap(), *value);
            }

            uint_histogram.clear();
            for (i, value) in [1, 1, 2, 0, 0, 0, 0, 0, 0].iter().enumerate() {
                assert_eq!(*value, block.array_get_uint_slot(i).unwrap());
            }

            let double_histogram = node.create_double_exponential_histogram(
                "double-histogram",
                ExponentialHistogramParams {
                    floor: 1.0,
                    initial_step: 1.0,
                    step_multiplier: 2.0,
                    buckets: 4,
                },
            );
            double_histogram.insert_multiple(0.0, 2); // underflow
            double_histogram.insert(8.3);
            double_histogram.insert(500.0); // overflow
            let block = double_histogram.get_block().unwrap();
            for (i, value) in [1.0, 1.0, 2.0, 2.0, 0.0, 0.0, 0.0, 1.0, 1.0].iter().enumerate() {
                assert_eq!(block.array_get_double_slot(i).unwrap(), *value);
            }

            assert_eq!(node_block.child_count().unwrap(), 3);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn owned_method_argument_properties() {
        let mapping = Arc::new(Mapping::allocate(4096).unwrap().0);
        let state = get_state(mapping.clone());
        let root = Node::new_root(state);
        let node = root.create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let _string_property =
                node.create_string(String::from("string_property"), String::from("test"));
            let _bytes_property =
                node.create_bytes(String::from("bytes_property"), vec![0, 1, 2, 3]);
            let _double_property = node.create_double(String::from("double_property"), 1.0);
            let _int_property = node.create_int(String::from("int_property"), 1);
            let _uint_property = node.create_uint(String::from("uint_property"), 1);
            assert_eq!(node_block.child_count().unwrap(), 5);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn dummy_partialeq() {
        let inspector = Inspector::new();
        let root = inspector.root();

        // Types should all be equal to another type. This is to enable clients
        // with inspect types in their structs be able to derive PartialEq and
        // Eq smoothly.
        assert_eq!(root, &root.create_child("child1"));
        assert_eq!(root.create_int("property1", 1), root.create_int("property2", 2));
        assert_eq!(root.create_double("property1", 1.0), root.create_double("property2", 2.0));
        assert_eq!(root.create_uint("property1", 1), root.create_uint("property2", 2));
        assert_eq!(
            root.create_string("property1", "value1"),
            root.create_string("property2", "value2")
        );
        assert_eq!(
            root.create_bytes("property1", b"value1"),
            root.create_bytes("property2", b"value2")
        );
    }

    fn get_state(mapping: Arc<Mapping>) -> State {
        let heap = Heap::new(mapping).unwrap();
        State::create(heap).expect("create state")
    }

    #[test]
    fn exp_histogram_insert() {
        let inspector = Inspector::new();
        let root = inspector.root();
        let hist = root.create_int_exponential_histogram(
            "test",
            ExponentialHistogramParams {
                floor: 0,
                initial_step: 2,
                step_multiplier: 4,
                buckets: 4,
            },
        );
        for i in -200..200 {
            hist.insert(i);
        }
        let block = hist.get_block().unwrap();
        assert_eq!(block.array_get_int_slot(0).unwrap(), 0);
        assert_eq!(block.array_get_int_slot(1).unwrap(), 2);
        assert_eq!(block.array_get_int_slot(2).unwrap(), 4);

        // Buckets
        let i = 3;
        assert_eq!(block.array_get_int_slot(i).unwrap(), 200);
        assert_eq!(block.array_get_int_slot(i + 1).unwrap(), 2);
        assert_eq!(block.array_get_int_slot(i + 2).unwrap(), 6);
        assert_eq!(block.array_get_int_slot(i + 3).unwrap(), 24);
        assert_eq!(block.array_get_int_slot(i + 4).unwrap(), 96);
        assert_eq!(block.array_get_int_slot(i + 5).unwrap(), 72);
    }

    #[test]
    fn lazy_values() {
        let inspector = Inspector::new();
        let node = inspector.root().create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let lazy_node =
                node.create_lazy_values("lazy", || async move { Ok(Inspector::new()) }.boxed());
            let lazy_node_block = lazy_node.get_block().unwrap();
            assert_eq!(lazy_node_block.block_type(), BlockType::LinkValue);
            assert_eq!(
                lazy_node_block.link_node_disposition().unwrap(),
                LinkNodeDisposition::Inline
            );
            assert_eq!(lazy_node_block.link_content_index().unwrap(), 5);
            assert_eq!(node_block.child_count().unwrap(), 1);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn lazy_node() {
        let inspector = Inspector::new();
        let node = inspector.root().create_child("node");
        let node_block = node.get_block().unwrap();
        {
            let lazy_node =
                node.create_lazy_child("lazy", || async move { Ok(Inspector::new()) }.boxed());
            let lazy_node_block = lazy_node.get_block().unwrap();
            assert_eq!(lazy_node_block.block_type(), BlockType::LinkValue);
            assert_eq!(
                lazy_node_block.link_node_disposition().unwrap(),
                LinkNodeDisposition::Child
            );
            assert_eq!(lazy_node_block.link_content_index().unwrap(), 5);
            assert_eq!(node_block.child_count().unwrap(), 1);
        }
        assert_eq!(node_block.child_count().unwrap(), 0);
    }

    #[test]
    fn inspector_lazy_from_vmo() {
        let inspector = Inspector::new();
        inspector.root().record_uint("test", 3);

        let embedded_inspector = Inspector::new();
        embedded_inspector.root().record_uint("test2", 4);
        let vmo = embedded_inspector.duplicate_vmo().unwrap();

        inspector.root().record_lazy_child_from_vmo("lazy", Arc::new(vmo));
        assert_data_tree!(inspector, root: {
            test: 3u64,
            lazy: {
                test2: 4u64,
            }
        });
    }

    #[test]
    fn value_list_record() {
        let inspector = Inspector::new();
        let child = inspector.root().create_child("test");
        let value_list = ValueList::new();
        assert!(value_list.values.lock().is_none());
        value_list.record(child);
        assert_eq!(value_list.values.lock().as_ref().unwrap().len(), 1);
    }

    #[test]
    fn record() {
        let inspector = Inspector::new();
        let property = inspector.root().create_uint("a", 1);
        inspector.root().record_uint("b", 2);
        {
            let child = inspector.root().create_child("child");
            child.record(property);
            child.record_double("c", 3.14);
            assert_data_tree!(inspector, root: {
                a: 1u64,
                b: 2u64,
                child: {
                    c: 3.14,
                }
            });
        }
        // `child` went out of scope, meaning it was deleted.
        // Property `a` should be gone as well, given that it was being tracked by `child`.
        assert_data_tree!(inspector, root: {
            b: 2u64,
        });
    }

    #[test]
    fn record_child() {
        let inspector = Inspector::new();
        inspector.root().record_child("test", |node| {
            node.record_int("a", 1);
        });
        assert_data_tree!(inspector, root: {
            test: {
                a: 1i64,
            }
        })
    }

    #[test]
    fn record_weak() {
        let inspector = Inspector::new();
        let main = inspector.root().create_child("main");
        let main_weak = main.clone_weak();
        let property = main_weak.create_uint("a", 1);

        // Ensure either the weak or strong reference can be used for recording
        main_weak.record_uint("b", 2);
        main.record_uint("c", 3);
        {
            let child = main_weak.create_child("child");
            child.record(property);
            child.record_double("c", 3.14);
            assert_data_tree!(inspector, root: { main: {
                a: 1u64,
                b: 2u64,
                c: 3u64,
                child: {
                    c: 3.14,
                }
            }});
        }
        // `child` went out of scope, meaning it was deleted.
        // Property `a` should be gone as well, given that it was being tracked by `child`.
        assert_data_tree!(inspector, root: { main: {
            b: 2u64,
            c: 3u64
        }});
        std::mem::drop(main);
        // Recording after dropping a strong reference is a no-op
        main_weak.record_double("d", 1.0);
        // Verify that dropping a strong reference cleans up the state
        assert_data_tree!(inspector, root: { });
    }

    #[test]
    fn unique_name() {
        let inspector = Inspector::new();

        let name_1 = super::unique_name("a");
        assert_eq!(name_1, "a0");
        inspector.root().record_uint(name_1, 1);

        let name_2 = super::unique_name("a");
        assert_eq!(name_2, "a1");
        inspector.root().record_uint(name_2, 1);

        assert_data_tree!(inspector, root: {
            a0: 1u64,
            a1: 1u64,
        });
    }

    #[fuchsia::test]
    async fn atomic_update_reader() {
        let inspector = Inspector::new();

        // Spawn a read thread that holds a duplicate handle to the VMO that will be written.
        let vmo = inspector.duplicate_vmo().expect("duplicate vmo handle");
        let (p1, p2) = zx::EventPair::create().unwrap();

        macro_rules! notify_and_wait_reader {
            () => {
                p1.signal_peer(zx::Signals::NONE, zx::Signals::USER_0).unwrap();
                p1.wait_handle(zx::Signals::USER_0, zx::Time::INFINITE).unwrap();
                p1.signal_handle(zx::Signals::USER_0, zx::Signals::NONE).unwrap();
            };
        }

        macro_rules! wait_and_notify_writer {
            ($code:block) => {
              p2.wait_handle(zx::Signals::USER_0, zx::Time::INFINITE).unwrap();
              p2.signal_handle(zx::Signals::USER_0, zx::Signals::NONE).unwrap();
              $code
              p2.signal_peer(zx::Signals::NONE, zx::Signals::USER_0).unwrap();
            }
        }

        let thread = std::thread::spawn(move || {
            // Before running the atomic update.
            wait_and_notify_writer! {{
                let hierarchy: DiagnosticsHierarchy<String> =
                    reader::PartialNodeHierarchy::try_from(&vmo).unwrap().into();
                assert_eq!(hierarchy, DiagnosticsHierarchy::new_root());
            }};
            // After: create_child("child"): Assert that the VMO is in use (locked) and we can't
            // read.
            wait_and_notify_writer! {{
                assert!(reader::PartialNodeHierarchy::try_from(&vmo).is_err());
            }};
            // After: record_int("a"): Assert that the VMO is in use (locked) and we can't
            // read.
            wait_and_notify_writer! {{
                assert!(reader::PartialNodeHierarchy::try_from(&vmo).is_err());
            }};
            // After: record_int("b"): Assert that the VMO is in use (locked) and we can't
            // read.
            wait_and_notify_writer! {{
                assert!(reader::PartialNodeHierarchy::try_from(&vmo).is_err());
            }};
            // After atomic update
            wait_and_notify_writer! {{
                let hierarchy: DiagnosticsHierarchy<String> =
                    reader::PartialNodeHierarchy::try_from(&vmo).unwrap().into();
                assert_data_tree!(hierarchy, root: {
                   value: 2i64,
                   child: {
                       a: 1i64,
                       b: 2i64,
                   }
                });
            }};
        });

        // Perform the atomic update
        let mut child = Node::default();
        notify_and_wait_reader!();
        let int_val = inspector.root().create_int("value", 1);
        inspector
            .root()
            .atomic_update(|node| {
                // Intentionally make this slow to assert an atomic update in the reader.
                child = node.create_child("child");
                notify_and_wait_reader!();
                child.record_int("a", 1);
                notify_and_wait_reader!();
                child.record_int("b", 2);
                notify_and_wait_reader!();
                int_val.add(1);
                Ok::<(), Error>(())
            })
            .expect("successful atomic update");
        notify_and_wait_reader!();

        // Wait for the reader thread to successfully finish.
        let _ = thread.join();

        // Ensure that the variable that we mutated internally can be used.
        child.record_int("c", 3);
        assert_data_tree!(inspector, root: {
            value: 2i64,
            child: {
                a: 1i64,
                b: 2i64,
                c: 3i64,
            }
        });
    }
}
