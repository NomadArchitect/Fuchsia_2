// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::mem;

use surpass;

use surpass::painter::Props;

const IDENTITY: &[f32; 6] = &[1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
const MAX_LAYER: usize = u16::MAX as usize;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LayerId(pub(crate) u16);

fn outer(index: usize) -> usize {
    index >> 6
}

fn inner(index: usize) -> usize {
    index & 0x3F
}

fn to_index(outer: usize, inner: usize) -> usize {
    (outer << 6) + inner
}

fn increment(index: usize) -> usize {
    index + 1 & MAX_LAYER
}

#[derive(Debug)]
pub(crate) struct LayerIdSet {
    bit_set: Vec<u64>,
    index: usize,
}

impl LayerIdSet {
    pub fn new() -> Self {
        Self { bit_set: vec![0; (MAX_LAYER + 1) >> 6], index: 0 }
    }

    fn set(&mut self, index: usize, value: bool) {
        let outer_index = outer(index);
        let inner_index = inner(index);

        let slot = &mut self.bit_set[outer_index];
        let mask = 0x1 << inner_index as u64;

        if value {
            *slot |= mask;
        } else {
            *slot &= !mask;
        }
    }

    fn next_free_slot(&self) -> Option<usize> {
        let index = self.index;
        let mut outer_index = outer(index);

        if self.bit_set[outer_index] != u64::max_value() {
            let inner_index = inner(index);
            let new_index = (!self.bit_set[outer_index]).trailing_zeros() as usize;

            if new_index >= inner_index {
                return Some(to_index(outer_index, new_index));
            }
        }

        outer_index = increment(outer_index);

        let mut slots = self.bit_set[outer_index..]
            .iter()
            .chain(self.bit_set[..outer_index].iter())
            .enumerate();

        slots.find_map(|(delta, &slot)| {
            if slot == u64::max_value() {
                return None;
            }

            Some(to_index(outer_index + delta, (!slot).trailing_zeros() as usize) & MAX_LAYER)
        })
    }

    pub fn create_id(&mut self) -> Option<LayerId> {
        self.next_free_slot().map(|index| {
            self.index = increment(index);
            self.set(index, true);
            LayerId(index as u16)
        })
    }

    pub fn remove(&mut self, id: LayerId) {
        self.set(id.0 as usize, false);
    }
}

type Container = u32;

#[derive(Clone, Debug, Default)]
pub struct SmallBitSet {
    bit_set: Container,
}

impl SmallBitSet {
    pub fn clear(&mut self) {
        self.bit_set = 0;
    }

    pub const fn contains(&self, val: &u8) -> bool {
        (self.bit_set >> *val as Container) & 0b1 != 0
    }

    pub fn insert(&mut self, val: u8) -> bool {
        if val as usize >= mem::size_of_val(&self.bit_set) * 8 {
            return false;
        }

        self.bit_set |= 0b1 << val as Container;

        true
    }

    pub fn remove(&mut self, val: u8) -> bool {
        if val as usize >= mem::size_of_val(&self.bit_set) * 8 {
            return false;
        }

        self.bit_set &= !(0b1 << val as Container);

        true
    }

    pub fn first_empty_slot(&mut self) -> Option<u8> {
        let slot = self.bit_set.trailing_ones() as u8;

        self.insert(slot).then(|| slot)
    }
}

#[derive(Debug, Default)]
pub struct Layer {
    pub(crate) inner: surpass::Layer,
    props: Props,
    pub(crate) is_unchanged: SmallBitSet,
    pub(crate) len: usize,
}

impl Layer {
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.inner.is_enabled
    }

    #[inline]
    pub fn set_is_enabled(&mut self, is_enabled: bool) -> &mut Self {
        self.inner.is_enabled = is_enabled;
        self
    }

    #[inline]
    pub fn disable(&mut self) -> &mut Self {
        self.inner.is_enabled = false;
        self
    }

    #[inline]
    pub fn enable(&mut self) -> &mut Self {
        self.inner.is_enabled = true;
        self
    }

    #[inline]
    pub fn transform(&self) -> &[f32; 6] {
        self.inner.affine_transform.as_ref().unwrap_or(IDENTITY)
    }

    #[inline]
    pub fn set_transform(&mut self, transform: &[f32; 6]) -> &mut Self {
        let affine_transform = if transform == IDENTITY {
            None
        } else {
            if transform[0] * transform[0] + transform[2] * transform[2] > 1.001
                || transform[1] * transform[1] + transform[3] * transform[3] > 1.001
            {
                panic!("Layer's scaling on each axis must be between -1.0 and 1.0");
            }

            Some(*transform)
        };

        if self.inner.affine_transform != affine_transform {
            self.is_unchanged.clear();
            self.inner.affine_transform = affine_transform;
        }

        self
    }

    #[inline]
    pub fn order(&self) -> u16 {
        self.inner.order.expect("Layers should always have orders")
    }

    #[inline]
    pub fn set_order(&mut self, order: u16) -> &mut Self {
        if self.inner.order != Some(order) {
            self.is_unchanged.clear();
            self.inner.order = Some(order);
        }

        self
    }

    #[inline]
    pub fn props(&self) -> &Props {
        &self.props
    }

    #[inline]
    pub fn set_props(&mut self, props: Props) -> &mut Self {
        if self.props != props {
            self.is_unchanged.clear();
            self.props = props;
        }

        self
    }

    pub(crate) fn is_unchanged(&self, cache_id: u8) -> bool {
        self.is_unchanged.contains(&cache_id)
    }

    pub(crate) fn set_is_unchanged(&mut self, cache_id: u8, is_unchanged: bool) -> bool {
        if is_unchanged {
            self.is_unchanged.insert(cache_id)
        } else {
            self.is_unchanged.remove(cache_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_id_get_set() {
        fn get(layer_id_set: &LayerIdSet, index: usize) -> bool {
            let outer_index = outer(index);
            let inner_index = inner(index);

            let slot = layer_id_set.bit_set[outer_index];

            (slot >> inner_index as u64) & 0x1 == 0x1
        }

        let mut layer_id_set = LayerIdSet::new();

        for &id in [0, 63, 64, 65_535].iter() {
            assert!(!get(&layer_id_set, id));

            layer_id_set.set(id, true);
            assert!(get(&layer_id_set, id));

            layer_id_set.set(id, false);
            assert!(!get(&layer_id_set, id));
        }
    }

    #[test]
    fn layer_id_next_free_slot_same_slot() {
        let mut layer_id_set = LayerIdSet::new();
        layer_id_set.bit_set[0] = 0x3FF;

        assert_eq!(layer_id_set.next_free_slot(), Some(10));
    }

    #[test]
    fn layer_id_next_free_slot_next_slot() {
        let mut layer_id_set = LayerIdSet::new();
        layer_id_set.bit_set[0] = u64::max_value();
        layer_id_set.bit_set[1] = 0x3FF;

        assert_eq!(layer_id_set.next_free_slot(), Some(64 + 10));
    }

    #[test]
    fn layer_id_next_free_slot_wrap_around() {
        let mut layer_id_set = LayerIdSet::new();

        for slot in &mut layer_id_set.bit_set {
            *slot = u64::max_value();
        }

        layer_id_set.bit_set[1] = 0x3FF;
        layer_id_set.index = 128;

        assert_eq!(layer_id_set.next_free_slot(), Some(64 + 10));
    }

    #[test]
    fn layer_id_create_first_and_last() {
        let mut layer_id_set = LayerIdSet::new();

        for slot in &mut layer_id_set.bit_set {
            *slot = u64::max_value();
        }

        layer_id_set.bit_set[0] = u64::max_value() ^ 0x1;
        *layer_id_set.bit_set.last_mut().unwrap() &= u64::max_value() >> 1;
        layer_id_set.index = 1;

        assert_eq!(layer_id_set.create_id(), Some(LayerId(MAX_LAYER as u16)));
        assert_eq!(layer_id_set.create_id(), Some(LayerId(0)));
        assert_eq!(layer_id_set.create_id(), None);
    }

    #[test]
    fn layer_id_next_free_slot_full() {
        let mut layer_id_set = LayerIdSet::new();

        for slot in &mut layer_id_set.bit_set {
            *slot = u64::max_value();
        }

        assert_eq!(layer_id_set.next_free_slot(), None);
    }
}
