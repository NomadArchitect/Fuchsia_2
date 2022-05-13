// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Defines generic data structures used to implement common application socket
//! functionality for multiple protocols.
//!
//! The core of this module is the [`SocketMap`] struct. It provides a map-like
//! API for setting and getting values while maintaining extra information about
//! the number of values of certain types present in the map.

use alloc::collections::{hash_map, HashMap};
use core::{hash::Hash, num::NonZeroUsize};

use const_unwrap::const_unwrap_option;
use derivative::Derivative;

/// A type whose values can "shadow" other values of the type.
///
/// An implementation of this trait defines a relationship between values of the
/// type. For any value `s: S`, if `t` appears in
/// `IterShadows::iter_shadows(s)`, then `s` shadows `t`.
///
/// This "shadows" relationship is similar to [`PartialOrd`] in that certain
/// propreties must hold:
///
/// 1. transitivity: if `s.iter_shadows()` yields `t`, and `t.iter_shadows()`
///    yields `u`, then `s.iter_shadows()` must also yield `u`.
/// 2. anticyclic: `s` cannot shadow itself.
///
/// Produces an iterator that yields all the shadows of a given value. The order
/// of iteration is unspecified.
pub trait IterShadows {
    type IterShadows: Iterator<Item = Self>;
    fn iter_shadows(&self) -> Self::IterShadows;
}

/// A type whose values can be used to produce "tag" values of a different type.
///
/// This can be used to provide a summary value, e.g. even or odd for an
/// integer-like type.
pub(crate) trait Tagged {
    type Tag: Copy + Eq + core::fmt::Debug;

    /// Returns the tag value for `self`.
    ///
    /// This function must be deterministic, such that calling `Tagged::Tag` on
    /// the same value always returns the same tag value.
    fn tag(&self) -> Self::Tag;
}

/// A map that stores values and summarizes tag counts.
///
/// This provides a similar insertion/removal API to [`HashMap`] for individual
/// key/value pairs. Unlike a regular `HashMap`, the key type `A` is required to
/// implement [`IterShadows`], and `V` to implement [`Tagged`].
///
/// Since `A` implements `IterShadows`, a given value `a : A` has zero or more
/// shadow values. Since the shadow relationship is transitive, we call any
/// value `v` that is reachable by following shadows of `a` one of `a`'s
/// "ancestors", and we say `a` is a "descendant" of `v`.
///
/// In addition to keys and values, this map stores the number of values
/// present in the map for all descendants of each key. These counts are
/// separated into buckets for different tags of type `V::Tag`.
#[derive(Derivative, Debug)]
#[derivative(Default(bound = ""))]
pub(crate) struct SocketMap<A: Hash + Eq, V: Tagged> {
    map: HashMap<A, MapValue<V>>,
    len: usize,
}

#[derive(Derivative, Debug)]
#[derivative(Default(bound = ""))]
struct MapValue<V: Tagged> {
    value: Option<V>,
    descendant_counts: DescendantCounts<V::Tag>,
}

#[derive(Derivative, Debug)]
#[derivative(Default(bound = ""))]
struct DescendantCounts<T, const INLINE_SIZE: usize = 1> {
    /// Holds unordered (tag, count) pairs.
    ///
    /// [`DescendantCounts`] maintains the invariant that tags are unique. The
    /// ordering of tags is unspecified.
    counts: smallvec::SmallVec<[(T, NonZeroUsize); INLINE_SIZE]>,
}

/// An entry for a key in a map that has a value.
///
/// This type maintains the invariant that, if an `OccupiedEntry(map, a)`
/// exists, `SocketMap::get(map, a)` is `Some(v)`, i.e. the `HashMap` that
/// [`SocketMap`] wraps contains a [`MapValue`] whose `value` field is
/// `Some(v)`.
#[cfg_attr(test, derive(Debug))]
pub(crate) struct OccupiedEntry<'a, A: Hash + Eq, V: Tagged>(&'a mut SocketMap<A, V>, A);

/// An entry for a key in a map that does not have a value.
///
/// This type maintains the invariant that, if a `VacantEntry(map, a)` exists,
/// `SocketMap::get(map, a)` is `None`. This means that in the `HashMap` that
/// `SocketMap` wraps, either there is no value for key `a` or there is a
/// `MapValue` whose `value` field is `None`.
#[cfg_attr(test, derive(Debug))]
pub(crate) struct VacantEntry<'a, A: Hash + Eq, V: Tagged>(&'a mut SocketMap<A, V>, A);

/// An entry in a map that can be used to manipulate the value in-place.
#[cfg_attr(test, derive(Debug))]
pub(crate) enum Entry<'a, A: Hash + Eq, V: Tagged> {
    // NB: Both `OccupiedEntry` and `VacantEntry` store a reference to the map
    // and a key directly since they need access to the entire map to update
    // descendant counts. This means that any operation on them requires an
    // additional map lookup with the same key. Experimentation suggests the
    // compiler will optimize this duplicate lookup out, since it is the same
    // one done by `SocketMap::entry` to produce the `Entry` in the first place.
    Occupied(OccupiedEntry<'a, A, V>),
    Vacant(VacantEntry<'a, A, V>),
}

impl<A, V> SocketMap<A, V>
where
    A: IterShadows + Hash + Eq,
    V: Tagged,
{
    /// Gets a reference to the value associated with the given key, if any.
    pub fn get(&self, key: &A) -> Option<&V> {
        let Self { map, len: _ } = self;
        map.get(key).and_then(|MapValue { value, descendant_counts: _ }| value.as_ref())
    }

    /// Provides an [`Entry`] for the given key for in-place manipulation.
    ///
    /// This is similar to the API provided by [`HashMap::entry`]. Callers can
    /// match on the result to perform different actions depending on whether
    /// the map has a value for the key or not.
    pub fn entry(&mut self, key: A) -> Entry<'_, A, V> {
        let Self { map, len: _ } = self;
        match map.get(&key) {
            Some(MapValue { descendant_counts: _, value: Some(_) }) => {
                Entry::Occupied(OccupiedEntry(self, key))
            }
            Some(MapValue { descendant_counts: _, value: None }) | None => {
                Entry::Vacant(VacantEntry(self, key))
            }
        }
    }

    /// Removes the value for the given key if there is one.
    ///
    /// If there is a value for key `key`, removes it and returns it. Otherwise
    /// returns None.
    pub fn remove(&mut self, key: &A) -> Option<V>
    where
        A: Clone,
    {
        let Self { map, len } = self;
        match map.entry(key.clone()) {
            hash_map::Entry::Vacant(_) => return None,
            hash_map::Entry::Occupied(mut o) => {
                let MapValue { descendant_counts, value } = o.get_mut();
                let value = value.take()?;
                if descendant_counts.is_empty() {
                    let _: MapValue<V> = o.remove();
                }
                Self::decrement_descendant_counts(map, key.iter_shadows(), value.tag());
                *len -= 1;
                Some(value)
            }
        }
    }

    /// Applies the provided function on the value for the given key.
    ///
    /// If the map has a value for `key`, calls `apply` on that value and then
    /// returns the result. Otherwise returns `None`.
    #[todo_unused::todo_unused("https://fxbug.dev/96320")]
    pub fn map_mut<R>(&mut self, key: &A, apply: impl FnOnce(&mut V) -> R) -> Option<R> {
        let Self { map, len: _ } = self;
        let value =
            map.get_mut(key).and_then(|MapValue { value, descendant_counts: _ }| value.as_mut())?;
        let old_tag = value.tag();
        let r = apply(value);
        let new_tag = value.tag();
        Self::update_descendant_counts(map, key.iter_shadows(), old_tag, new_tag);
        Some(r)
    }

    /// Returns counts of tags for values at keys that shadow `key`.
    ///
    /// This is equivalent to iterating over all keys in the map, filtering for
    /// those keys for which `key` is one of their shadows, then calling
    /// [`Tagged::tag`] on the value for each of those keys, and then computing
    /// the number of occurrences for each tag.
    pub fn descendant_counts(
        &self,
        key: &A,
    ) -> impl ExactSizeIterator<Item = &'_ (V::Tag, NonZeroUsize)> {
        let Self { map, len: _ } = self;
        OptionalIterator(
            map.get(key)
                .map(|MapValue { value: _, descendant_counts }| descendant_counts.into_iter()),
        )
    }

    /// Returns an iterator over the keys and values in the map.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&'_ A, &'_ V)> {
        let Self { map, len: _ } = self;
        map.iter().filter_map(|(a, MapValue { value, descendant_counts: _ })| {
            value.as_ref().map(|v| (a, v))
        })
    }

    fn increment_descendant_counts(
        map: &mut HashMap<A, MapValue<V>>,
        shadows: A::IterShadows,
        tag: V::Tag,
    ) {
        for shadow in shadows {
            let MapValue { descendant_counts, value: _ } = map.entry(shadow).or_default();
            descendant_counts.increment(tag);
        }
    }

    #[todo_unused::todo_unused("https://fxbug.dev/96320")]
    fn update_descendant_counts(
        map: &mut HashMap<A, MapValue<V>>,
        shadows: A::IterShadows,
        old_tag: V::Tag,
        new_tag: V::Tag,
    ) {
        if old_tag != new_tag {
            for shadow in shadows {
                let counts = &mut map.get_mut(&shadow).unwrap().descendant_counts;
                counts.increment(new_tag);
                counts.decrement(old_tag);
            }
        }
    }

    fn decrement_descendant_counts(
        map: &mut HashMap<A, MapValue<V>>,
        shadows: A::IterShadows,
        old_tag: V::Tag,
    ) {
        for shadow in shadows {
            let mut entry = match map.entry(shadow) {
                hash_map::Entry::Occupied(o) => o,
                hash_map::Entry::Vacant(_) => unreachable!(),
            };
            let MapValue { descendant_counts, value } = entry.get_mut();
            descendant_counts.decrement(old_tag);
            if descendant_counts.is_empty() && value.is_none() {
                let _: MapValue<_> = entry.remove();
            }
        }
    }
}

#[todo_unused::todo_unused("https://fxbug.dev/96320")]
impl<'a, K: Eq + Hash + IterShadows, V: Tagged> OccupiedEntry<'a, K, V> {
    /// Retrieves the value referenced by this entry.
    #[todo_unused::todo_unused("https://fxbug.dev/96320")]
    pub(crate) fn get(&self) -> &V {
        let Self(SocketMap { map, len: _ }, key) = self;
        let MapValue { descendant_counts: _, value } = map.get(key).unwrap();
        // unwrap() call is guaranteed safe by OccupiedEntry invariant.
        value.as_ref().unwrap()
    }

    // NB: there is no get_mut because that would allow the caller to manipulate
    // a value without updating the descendant tag counts.

    /// Runs the provided callback on the value referenced by this entry.
    ///
    /// Returns the result of the callback.
    #[todo_unused::todo_unused("https://fxbug.dev/96320")]
    pub(crate) fn map_mut<R>(&mut self, apply: impl FnOnce(&mut V) -> R) -> R {
        let Self(SocketMap { map, len: _ }, key) = self;
        // unwrap() calls are guaranteed safe by OccupiedEntry invariant.
        let MapValue { descendant_counts: _, value } = map.get_mut(key).unwrap();
        let value = value.as_mut().unwrap();

        let old_tag = value.tag();
        let r = apply(value);
        let new_tag = value.tag();
        SocketMap::update_descendant_counts(map, key.iter_shadows(), old_tag, new_tag);
        r
    }
}

impl<'a, K: Eq + Hash + IterShadows, V: Tagged> VacantEntry<'a, K, V> {
    /// Inserts a value for the key referenced by this entry.
    pub(crate) fn insert(self, value: V) {
        let Self(SocketMap { map, len }, key) = self;
        let iter_shadows = key.iter_shadows();
        let MapValue { value: map_value, descendant_counts: _ } = map.entry(key).or_default();
        let tag = value.tag();
        assert!(map_value.replace(value).is_none());
        *len += 1;
        SocketMap::increment_descendant_counts(map, iter_shadows, tag);
    }

    /// Gets the descendant counts for this entry.
    pub(crate) fn descendant_counts(
        &self,
    ) -> impl ExactSizeIterator<Item = &'_ (V::Tag, NonZeroUsize)> {
        let Self(socket_map, key) = self;
        socket_map.descendant_counts(&key)
    }
}

impl<T: Eq, const INLINE_SIZE: usize> DescendantCounts<T, INLINE_SIZE> {
    const ONE: NonZeroUsize = const_unwrap_option(NonZeroUsize::new(1));

    /// Increments the count for the given tag.
    fn increment(&mut self, tag: T) {
        let Self { counts } = self;
        match counts.iter_mut().find_map(|(t, count)| (t == &tag).then(|| count)) {
            Some(count) => *count = NonZeroUsize::new(count.get() + 1).unwrap(),
            None => counts.push((tag, Self::ONE)),
        }
    }

    /// Decrements the count for the given tag.
    ///
    /// # Panics
    ///
    /// Panics if there is no count for the given tag.
    fn decrement(&mut self, tag: T) {
        let Self { counts } = self;
        let (index, count) = counts
            .iter_mut()
            .enumerate()
            .find_map(|(i, (t, count))| (t == &tag).then(|| (i, count)))
            .unwrap();
        if let Some(new_count) = NonZeroUsize::new(count.get() - 1) {
            *count = new_count
        } else {
            let _: (T, NonZeroUsize) = counts.swap_remove(index);
        }
    }

    fn is_empty(&self) -> bool {
        let Self { counts } = self;
        counts.is_empty()
    }
}

impl<'d, T, const INLINE_SIZE: usize> IntoIterator for &'d DescendantCounts<T, INLINE_SIZE> {
    type Item = &'d (T, NonZeroUsize);
    type IntoIter =
        <&'d smallvec::SmallVec<[(T, NonZeroUsize); INLINE_SIZE]> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        let DescendantCounts { counts } = self;
        counts.into_iter()
    }
}

/// Wrapper for an optional iterator.
struct OptionalIterator<I>(Option<I>);

impl<I: Iterator> Iterator for OptionalIterator<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        let Self(it) = self;
        it.as_mut().and_then(Iterator::next)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let Self(it) = self;
        it.as_ref().map(Iterator::size_hint).unwrap_or((0, Some(0)))
    }
}

impl<I: ExactSizeIterator> ExactSizeIterator for OptionalIterator<I> {}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use assert_matches::assert_matches;
    use proptest::strategy::Strategy;

    use super::*;

    trait AsMap {
        type K: Hash + Eq;
        type V;
        fn as_map(self) -> HashMap<Self::K, Self::V>;
    }

    impl<'d, K, V, I> AsMap for I
    where
        K: Hash + Eq + Clone + 'd,
        V: 'd,
        V: Clone + Into<usize>,
        I: Iterator<Item = &'d (K, V)>,
    {
        type K = K;
        type V = usize;
        fn as_map(self) -> HashMap<Self::K, Self::V> {
            self.map(|(k, v)| (k.clone(), v.clone().into())).collect()
        }
    }

    #[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
    enum Address {
        A(u8),
        AB(u8, char),
        ABC(u8, char, u8),
    }
    use Address::*;

    impl IterShadows for Address {
        type IterShadows = <Vec<Address> as IntoIterator>::IntoIter;
        fn iter_shadows(&self) -> Self::IterShadows {
            match self {
                A(_) => vec![],
                AB(a, _) => vec![A(*a)],
                ABC(a, b, _) => vec![AB(*a, *b), A(*a)],
            }
            .into_iter()
        }
    }

    #[derive(Eq, PartialEq, Clone, Copy, Debug)]
    struct TV<T, V>(T, V);

    impl<T: Copy + Eq + core::fmt::Debug, V> Tagged for TV<T, V> {
        type Tag = T;

        fn tag(&self) -> Self::Tag {
            self.0
        }
    }

    type TestSocketMap<T> = SocketMap<Address, TV<T, u8>>;

    #[test]
    fn insert_get_remove() {
        let mut map = TestSocketMap::default();

        assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Vacant(v) => v.insert(TV(0, 32)));
        assert_eq!(map.get(&ABC(1, 'c', 2)), Some(&TV(0, 32)));

        assert_eq!(map.remove(&ABC(1, 'c', 2)), Some(TV(0, 32)));
        assert_eq!(map.get(&ABC(1, 'c', 2)), None);
    }

    #[test]
    fn insert_remove_len() {
        let mut map = TestSocketMap::default();
        let TestSocketMap { len, map: _ } = map;
        assert_eq!(len, 0);

        assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Vacant(v) => v.insert(TV(0, 32)));
        let TestSocketMap { len, map: _ } = map;
        assert_eq!(len, 1);

        assert_eq!(map.remove(&ABC(1, 'c', 2)), Some(TV(0, 32)));
        let TestSocketMap { len, map: _ } = map;
        assert_eq!(len, 0);
    }

    #[test]
    fn entry_same_key() {
        let mut map = TestSocketMap::default();

        assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Vacant(v) => v.insert(TV(0, 32)));
        let occupied = assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Occupied(o) => o);
        assert_eq!(occupied.get(), &TV(0, 32));
        let TestSocketMap { len, map: _ } = map;
        assert_eq!(len, 1);
    }

    #[test]
    fn multiple_insert_descendant_counts() {
        let mut map = TestSocketMap::default();

        assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Vacant(v) => v.insert(TV(1, 111)));
        assert_matches!(map.entry(ABC(1, 'd', 2)), Entry::Vacant(v) => v.insert(TV(2, 111)));
        assert_matches!(map.entry(AB(5, 'd')), Entry::Vacant(v) => v.insert(TV(1, 54)));
        assert_matches!(map.entry(AB(1, 'd')),  Entry::Vacant(v) => v.insert(TV(3, 56)));
        let TestSocketMap { len, map: _ } = map;
        assert_eq!(len, 4);

        assert_eq!(map.descendant_counts(&A(1)).as_map(), HashMap::from([(1, 1), (2, 1), (3, 1)]));
        assert_eq!(map.descendant_counts(&AB(1, 'c')).as_map(), HashMap::from([(1, 1)]));
        assert_eq!(map.descendant_counts(&AB(1, 'd')).as_map(), HashMap::from([(2, 1)]));

        assert_eq!(map.descendant_counts(&A(5)).as_map(), HashMap::from([(1, 1)]));

        assert_eq!(map.descendant_counts(&ABC(1, 'd', 2)).as_map(), HashMap::from([]));
        assert_eq!(map.descendant_counts(&A(2)).as_map(), HashMap::from([]));
    }

    #[test]
    fn map_mut_keep_descendant_counts() {
        let mut map = TestSocketMap::default();

        assert_matches!(map.entry(A(1)), Entry::Vacant(v) => v.insert(TV(3, 56)));
        assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Vacant(v) => v.insert(TV(3, 111)));
        let expected_counts = HashMap::from([(3, 1)]);
        assert_eq!(map.descendant_counts(&A(1)).as_map(), expected_counts);

        assert_eq!(map.map_mut(&ABC(1, 'c', 2), |TV(_, v)| *v = 255), Some(()));
        assert_eq!(map.descendant_counts(&A(1)).as_map(), expected_counts);
    }

    #[test]
    fn map_mut_change_descendant_counts() {
        let mut map = TestSocketMap::default();

        assert_matches!(map.entry(A(1)), Entry::Vacant(v) => v.insert(TV(3, 56)));
        assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Vacant(v) => v.insert(TV(3, 111)));
        assert_eq!(map.descendant_counts(&A(1)).as_map(), HashMap::from([(3, 1)]));

        assert_eq!(map.map_mut(&ABC(1, 'c', 2), |TV(t, _)| *t = 80), Some(()));
        assert_eq!(map.descendant_counts(&A(1)).as_map(), HashMap::from([(80, 1)]));
    }

    #[test]
    fn map_mut_value_not_present() {
        let mut map = TestSocketMap::default();

        assert_matches!(map.entry(ABC(1, 'c', 2)), Entry::Vacant(v) => v.insert(TV(3, 111)));
        assert_eq!(map.map_mut(&ABC(32, 'g', 27), |TV(_, _)| 3245), None);
    }

    #[test]
    fn map_mut_passes_return_value_when_present() {
        let mut map = TestSocketMap::default();

        assert_matches!(map.entry(ABC(16, 'c', 8)), Entry::Vacant(v) => v.insert(TV(3, 111)));
        assert_eq!(map.map_mut(&ABC(16, 'c', 8), |TV(_, _)| 1845859), Some(1845859));
    }

    #[test]
    fn remove_ancestor_value() {
        let mut map = TestSocketMap::default();
        assert_matches!(map.entry(ABC(2, 'e', 1)), Entry::Vacant(v) => v.insert(TV(20, 100)));
        assert_matches!(map.entry(AB(2, 'e')), Entry::Vacant(v) => v.insert(TV(20, 100)));
        assert_eq!(map.remove(&AB(2, 'e')), Some(TV(20, 100)));

        assert_eq!(map.descendant_counts(&A(2)).as_map(), HashMap::from([(20, 1)]));
    }

    fn key_strategy() -> impl Strategy<Value = Address> {
        let a_strategy = 1..5u8;
        let b_strategy = proptest::char::range('a', 'e');
        let c_strategy = 1..5u8;
        (a_strategy, proptest::option::of((b_strategy, proptest::option::of(c_strategy)))).prop_map(
            |(a, b)| match b {
                None => A(a),
                Some((b, None)) => AB(a, b),
                Some((b, Some(c))) => ABC(a, b, c),
            },
        )
    }

    fn value_strategy() -> impl Strategy<Value = TV<u8, u8>> {
        (20..25u8, 100..105u8).prop_map(|(t, v)| TV(t, v))
    }

    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    enum Operation {
        Entry(Address, TV<u8, u8>),
        Replace(Address, TV<u8, u8>),
        Remove(Address),
    }

    impl Operation {
        fn apply(
            self,
            socket_map: &mut TestSocketMap<u8>,
            reference: &mut HashMap<Address, TV<u8, u8>>,
        ) {
            match self {
                Operation::Entry(a, v) => match (socket_map.entry(a), reference.entry(a)) {
                    (Entry::Occupied(mut s), hash_map::Entry::Occupied(mut h)) => {
                        assert_eq!(s.map_mut(|value| core::mem::replace(value, v)), h.insert(v))
                    }
                    (Entry::Vacant(s), hash_map::Entry::Vacant(h)) => {
                        s.insert(v);
                        let _: &mut TV<_, _> = h.insert(v);
                    }
                    (Entry::Occupied(_), hash_map::Entry::Vacant(_)) => {
                        panic!("socketmap has a value for {:?} but reference does not", a)
                    }
                    (Entry::Vacant(_), hash_map::Entry::Occupied(_)) => {
                        panic!("socketmap has no value for {:?} but reference does", a)
                    }
                },
                Operation::Replace(a, v) => {
                    match socket_map.map_mut(&a, |x| core::mem::replace(x, v)) {
                        Some(prev_v) => assert_eq!(reference.insert(a, v), Some(prev_v)),
                        None => assert_eq!(reference.get(&a), None),
                    }
                }
                Operation::Remove(a) => assert_eq!(socket_map.remove(&a), reference.remove(&a)),
            }
        }
    }

    fn operation_strategy() -> impl Strategy<Value = Operation> {
        proptest::prop_oneof!(
            (key_strategy(), value_strategy()).prop_map(|(a, v)| Operation::Entry(a, v)),
            (key_strategy(), value_strategy()).prop_map(|(a, v)| Operation::Replace(a, v)),
            key_strategy().prop_map(|a| Operation::Remove(a)),
        )
    }

    fn validate_map(map: TestSocketMap<u8>, reference: HashMap<Address, TV<u8, u8>>) {
        let map_values: HashMap<_, _> = map.iter().map(|(a, v)| (*a, *v)).collect();
        assert_eq!(map_values, reference);
        let TestSocketMap { len, map: _ } = map;
        assert_eq!(len, reference.len());

        let TestSocketMap { map: inner_map, len: _ } = &map;
        for (key, entry) in inner_map {
            let descendant_values = map
                .iter()
                .filter(|(k, _)| k.iter_shadows().any(|s| s == *key))
                .map(|(_, value)| value);

            // Fold values into a map from tag to count.
            let expected_tag_counts = descendant_values.fold(HashMap::new(), |mut m, v| {
                *m.entry(v.tag()).or_default() += 1;
                m
            });

            let MapValue { descendant_counts, value: _ } = entry;
            assert_eq!(
                expected_tag_counts,
                descendant_counts.into_iter().as_map(),
                "key = {key:?}"
            );
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            // Add all failed seeds here.
            failure_persistence: proptest_support::failed_seeds!(),
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn test_arbitrary_operations(operations in proptest::collection::vec(operation_strategy(), 10)) {
            let mut map = TestSocketMap::default();
            let mut reference = HashMap::new();
            for op in operations {
                op.apply(&mut map, &mut reference);
            }

            // After all operations have completed, check invariants for
            // SocketMap.
            validate_map(map, reference);
        }

    }
}
