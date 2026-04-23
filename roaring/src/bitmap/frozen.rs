use core::cmp::Ordering;

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, vec::Vec};

use crate::{
    bitmap::{
        container::Container,
        store::{ArrayStore, BitmapStore, Interval, IntervalStore, Store, BITMAP_LENGTH},
    },
    RoaringBitmap,
};

/// The cookie value used in the header of the frozen format to identify it as a valid frozen bitmap.
pub const FROZEN_COOKIE: u32 = 13746;

/// Type code for bitmap containers in the frozen format.
pub const BITSET_CONTAINER_TYPE: u8 = 1;

/// Type code for run containers in the frozen format.
pub const RUN_CONTAINER_TYPE: u8 = 2;

/// Type code for array containers in the frozen format.
pub const ARRAY_CONTAINER_TYPE: u8 = 3;

impl RoaringBitmap {
    /// Returns the number of bytes required to serialize the bitmap in the frozen format.
    #[must_use]
    pub fn frozen_size_in_bytes(&self) -> usize {
        let mut num_bytes = 0;
        for c in &self.containers {
            match c.store {
                Store::Bitmap(_) => num_bytes += BITMAP_LENGTH * 8,
                Store::Run(ref runs) => num_bytes += runs.run_amount() as usize * 4,
                Store::Array(ref a) => num_bytes += a.len() as usize * 2,
            }
        }
        num_bytes += 5 * self.containers.len();
        num_bytes += 4; // header
        num_bytes
    }

    /// Serializes the bitmap in the frozen format into the provided buffer.
    /// The buffer must be at least `frozen_size_in_bytes()` large.
    pub fn frozen_serialize_into(&self, buf: &mut [u8]) {
        let mut bitset_size = 0;
        let mut run_size = 0;
        let mut array_size = 0;

        for c in &self.containers {
            match c.store {
                Store::Bitmap(_) => bitset_size += BITMAP_LENGTH * 8,
                Store::Run(ref runs) => run_size += runs.run_amount() as usize * 4,
                Store::Array(ref a) => array_size += a.len() as usize * 2,
            }
        }

        let mut bitset_ptr = 0;
        let mut run_ptr = bitset_size;
        let mut array_ptr = bitset_size + run_size;
        let mut keys_ptr = bitset_size + run_size + array_size;
        let mut counts_ptr = keys_ptr + 2 * self.containers.len();
        let typecodes_ptr = counts_ptr + 2 * self.containers.len();
        let header_ptr = typecodes_ptr + self.containers.len();

        for (typecodes_ptr, c) in
            (counts_ptr + 2 * self.containers.len()..).zip(self.containers.iter())
        {
            let key_bytes = c.key.to_le_bytes();
            buf[keys_ptr..keys_ptr + 2].copy_from_slice(&key_bytes);
            keys_ptr += 2;

            match c.store {
                Store::Bitmap(ref b) => {
                    for &w in b.as_array() {
                        buf[bitset_ptr..bitset_ptr + 8].copy_from_slice(&w.to_le_bytes());
                        bitset_ptr += 8;
                    }
                    let count = (b.len() - 1) as u16;
                    buf[counts_ptr..counts_ptr + 2].copy_from_slice(&count.to_le_bytes());
                    buf[typecodes_ptr] = BITSET_CONTAINER_TYPE;
                }
                Store::Run(ref runs) => {
                    for iv in runs.iter_intervals() {
                        buf[run_ptr..run_ptr + 2].copy_from_slice(&iv.start().to_le_bytes());
                        let len = iv.end() - iv.start(); // Roaring stores len minus 1
                        buf[run_ptr + 2..run_ptr + 4].copy_from_slice(&len.to_le_bytes());
                        run_ptr += 4;
                    }
                    let count = runs.run_amount() as u16;
                    buf[counts_ptr..counts_ptr + 2].copy_from_slice(&count.to_le_bytes());
                    buf[typecodes_ptr] = RUN_CONTAINER_TYPE;
                }
                Store::Array(ref a) => {
                    for &val in a.iter() {
                        buf[array_ptr..array_ptr + 2].copy_from_slice(&val.to_le_bytes());
                        array_ptr += 2;
                    }
                    let count = (a.len() - 1) as u16;
                    buf[counts_ptr..counts_ptr + 2].copy_from_slice(&count.to_le_bytes());
                    buf[typecodes_ptr] = ARRAY_CONTAINER_TYPE;
                }
            }
            counts_ptr += 2;
        }

        let header = ((self.containers.len() as u32) << 15) | FROZEN_COOKIE;
        buf[header_ptr..header_ptr + 4].copy_from_slice(&header.to_le_bytes());
    }
}

/// A read-only view of a `RoaringBitmap` serialized in the frozen format.
#[derive(Clone, Copy)]
pub struct FrozenRoaringBitmapView<'a> {
    num_containers: usize,
    keys: &'a [u8],
    counts: &'a [u8],
    typecodes: &'a [u8],
    bitset_zone: &'a [u8],
    run_zone: &'a [u8],
    array_zone: &'a [u8],
}

impl<'a> FrozenRoaringBitmapView<'a> {
    /// Creates a new view from a byte slice.
    /// The buffer must be aligned by 32 bytes and contain a valid frozen bitmap.
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Option<Self> {
        if !(buf.as_ptr() as usize).is_multiple_of(32) {
            return None;
        }
        Self::new_unaligned(buf)
    }

    /// Creates a new view from a byte slice without verifying alignment.
    #[must_use]
    pub fn new_unaligned(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < 4 {
            return None;
        }
        let header = u32::from_le_bytes(buf[buf.len() - 4..].try_into().unwrap());
        if (header & 0x7FFF) != FROZEN_COOKIE {
            return None;
        }
        let num_containers = (header >> 15) as usize;
        let md_size = 4 + num_containers * 5;
        if buf.len() < md_size {
            return None;
        }

        let keys_ptr = buf.len() - 4 - num_containers * 5;
        let counts_ptr = buf.len() - 4 - num_containers * 3;
        let typecodes_ptr = buf.len() - 4 - num_containers;

        let mut bitset_size = 0;
        let mut run_size = 0;
        let mut array_size = 0;

        for i in 0..num_containers {
            let tc = buf[typecodes_ptr + i];
            let count = u16::from_le_bytes(
                buf[counts_ptr + i * 2..counts_ptr + i * 2 + 2].try_into().unwrap(),
            ) as usize;
            match tc {
                BITSET_CONTAINER_TYPE => bitset_size += BITMAP_LENGTH * 8,
                RUN_CONTAINER_TYPE => run_size += count * 4,
                ARRAY_CONTAINER_TYPE => array_size += (count + 1) * 2,
                _ => return None,
            }
        }

        if buf.len() < bitset_size + run_size + array_size + md_size {
            return None;
        }

        Some(Self {
            num_containers,
            keys: &buf[keys_ptr..keys_ptr + num_containers * 2],
            counts: &buf[counts_ptr..counts_ptr + num_containers * 2],
            typecodes: &buf[typecodes_ptr..typecodes_ptr + num_containers],
            bitset_zone: &buf[0..bitset_size],
            run_zone: &buf[bitset_size..bitset_size + run_size],
            array_zone: &buf[bitset_size + run_size..bitset_size + run_size + array_size],
        })
    }

    /// Converts this frozen view back into a fully owned `RoaringBitmap`.
    #[must_use]
    pub fn to_roaring_bitmap(&self) -> RoaringBitmap {
        let mut containers = Vec::with_capacity(self.num_containers);
        let mut bitset_ptr = 0;
        let mut run_ptr = 0;
        let mut array_ptr = 0;

        for i in 0..self.num_containers {
            let key = u16::from_le_bytes(self.keys[i * 2..i * 2 + 2].try_into().unwrap());
            let count =
                u16::from_le_bytes(self.counts[i * 2..i * 2 + 2].try_into().unwrap()) as usize;
            let tc = self.typecodes[i];

            let store = match tc {
                BITSET_CONTAINER_TYPE => {
                    let mut bits = Box::new([0u64; BITMAP_LENGTH]);
                    for j in 0..BITMAP_LENGTH {
                        let offset = bitset_ptr + j * 8;
                        bits[j] = u64::from_le_bytes(
                            self.bitset_zone[offset..offset + 8].try_into().unwrap(),
                        );
                    }
                    bitset_ptr += BITMAP_LENGTH * 8;
                    Store::Bitmap(BitmapStore::from_unchecked(count as u64 + 1, bits))
                }
                RUN_CONTAINER_TYPE => {
                    let mut runs = Vec::with_capacity(count);
                    for _ in 0..count {
                        let start = u16::from_le_bytes(
                            self.run_zone[run_ptr..run_ptr + 2].try_into().unwrap(),
                        );
                        let len = u16::from_le_bytes(
                            self.run_zone[run_ptr + 2..run_ptr + 4].try_into().unwrap(),
                        );
                        runs.push(Interval::new_unchecked(start, start.saturating_add(len)));
                        run_ptr += 4;
                    }
                    Store::Run(IntervalStore::from_vec_unchecked(runs))
                }
                ARRAY_CONTAINER_TYPE => {
                    let mut array = Vec::with_capacity(count + 1);
                    for _ in 0..=count {
                        let val = u16::from_le_bytes(
                            self.array_zone[array_ptr..array_ptr + 2].try_into().unwrap(),
                        );
                        array.push(val);
                        array_ptr += 2;
                    }
                    Store::Array(ArrayStore::from_vec_unchecked(array))
                }
                _ => unreachable!(),
            };

            containers.push(Container { key, store });
        }

        RoaringBitmap { containers }
    }

    /// Checks if the bitmap is empty by verifying that all containers are empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.num_containers == 0
    }

    /// Returns the number of elements in the bitmap.
    #[must_use]
    pub fn len(&self) -> u64 {
        let mut total = 0;
        let mut run_offset = 0;
        for i in 0..self.num_containers {
            let count =
                u16::from_le_bytes(self.counts[i * 2..i * 2 + 2].try_into().unwrap()) as u64;
            match self.typecodes[i] {
                BITSET_CONTAINER_TYPE | ARRAY_CONTAINER_TYPE => total += count + 1,
                RUN_CONTAINER_TYPE => {
                    for _ in 0..count {
                        let len = u16::from_le_bytes(
                            self.run_zone[run_offset + 2..run_offset + 4].try_into().unwrap(),
                        ) as u64;
                        total += len + 1;
                        run_offset += 4;
                    }
                }
                _ => {}
            }
        }
        total
    }

    /// Returns the minimum value in the bitmap, or `None` if the bitmap is empty.
    #[must_use]
    pub fn min(&self) -> Option<u32> {
        if self.num_containers == 0 {
            return None;
        }
        let key = u16::from_le_bytes(self.keys[0..2].try_into().unwrap());

        let tc = self.typecodes[0];
        match tc {
            BITSET_CONTAINER_TYPE => {
                for i in 0..BITMAP_LENGTH {
                    let offset = i * 8;
                    let word = u64::from_le_bytes(
                        self.bitset_zone[offset..offset + 8].try_into().unwrap(),
                    );
                    if word != 0 {
                        return Some(
                            ((key as u32) << 16) | ((i as u32 * 64) + word.trailing_zeros()),
                        );
                    }
                }
                None
            }
            RUN_CONTAINER_TYPE => {
                let start = u16::from_le_bytes(self.run_zone[0..2].try_into().unwrap());
                Some(((key as u32) << 16) | (start as u32))
            }
            ARRAY_CONTAINER_TYPE => {
                let val = u16::from_le_bytes(self.array_zone[0..2].try_into().unwrap());
                Some(((key as u32) << 16) | (val as u32))
            }
            _ => None,
        }
    }

    /// Returns the maximum value in the bitmap, or `None` if the bitmap is empty.
    #[must_use]
    pub fn max(&self) -> Option<u32> {
        if self.num_containers == 0 {
            return None;
        }
        let i = self.num_containers - 1;
        let key = u16::from_le_bytes(self.keys[i * 2..i * 2 + 2].try_into().unwrap());

        let tc = self.typecodes[i];
        let mut bitset_offset = 0;
        let mut run_offset = 0;
        let mut array_offset = 0;

        for j in 0..i {
            let c = u16::from_le_bytes(self.counts[j * 2..j * 2 + 2].try_into().unwrap()) as usize;
            match self.typecodes[j] {
                BITSET_CONTAINER_TYPE => bitset_offset += BITMAP_LENGTH * 8,
                RUN_CONTAINER_TYPE => run_offset += c * 4,
                ARRAY_CONTAINER_TYPE => array_offset += (c + 1) * 2,
                _ => {}
            }
        }

        let count = u16::from_le_bytes(self.counts[i * 2..i * 2 + 2].try_into().unwrap()) as usize;
        match tc {
            BITSET_CONTAINER_TYPE => {
                for word_idx in (0..BITMAP_LENGTH).rev() {
                    let offset = bitset_offset + word_idx * 8;
                    let word = u64::from_le_bytes(
                        self.bitset_zone[offset..offset + 8].try_into().unwrap(),
                    );
                    if word != 0 {
                        let bit_idx = word.ilog2();
                        return Some(((key as u32) << 16) | ((word_idx as u32 * 64) + bit_idx));
                    }
                }
                None
            }
            RUN_CONTAINER_TYPE => {
                let offset = run_offset + (count - 1) * 4;
                let start =
                    u16::from_le_bytes(self.run_zone[offset..offset + 2].try_into().unwrap());
                let len =
                    u16::from_le_bytes(self.run_zone[offset + 2..offset + 4].try_into().unwrap());
                let end = start.saturating_add(len);
                Some(((key as u32) << 16) | (end as u32))
            }
            ARRAY_CONTAINER_TYPE => {
                let offset = array_offset + count * 2;
                let val =
                    u16::from_le_bytes(self.array_zone[offset..offset + 2].try_into().unwrap());
                Some(((key as u32) << 16) | (val as u32))
            }
            _ => None,
        }
    }

    /// Checks if the bitmap contains the specified value.
    #[must_use]
    pub fn contains(&self, value: u32) -> bool {
        let key = (value >> 16) as u16;
        let index = value as u16;

        let mut left = 0;
        let mut right = self.num_containers;
        while left < right {
            let mid = left + (right - left) / 2;
            let mid_key = u16::from_le_bytes(self.keys[mid * 2..mid * 2 + 2].try_into().unwrap());
            match mid_key.cmp(&key) {
                Ordering::Less => left = mid + 1,
                Ordering::Greater => right = mid,
                Ordering::Equal => return self.container_contains(mid, index),
            }
        }
        false
    }

    fn container_contains(&self, i: usize, index: u16) -> bool {
        let tc = self.typecodes[i];
        let mut bitset_offset = 0;
        let mut run_offset = 0;
        let mut array_offset = 0;

        for j in 0..i {
            let c = u16::from_le_bytes(self.counts[j * 2..j * 2 + 2].try_into().unwrap()) as usize;
            match self.typecodes[j] {
                BITSET_CONTAINER_TYPE => bitset_offset += BITMAP_LENGTH * 8,
                RUN_CONTAINER_TYPE => run_offset += c * 4,
                ARRAY_CONTAINER_TYPE => array_offset += (c + 1) * 2,
                _ => {}
            }
        }

        let count = u16::from_le_bytes(self.counts[i * 2..i * 2 + 2].try_into().unwrap()) as usize;
        match tc {
            BITSET_CONTAINER_TYPE => {
                let word_idx = (index / 64) as usize;
                let bit_idx = index % 64;
                let offset = bitset_offset + word_idx * 8;
                let word =
                    u64::from_le_bytes(self.bitset_zone[offset..offset + 8].try_into().unwrap());
                (word & (1 << bit_idx)) != 0
            }
            RUN_CONTAINER_TYPE => {
                let mut left = 0;
                let mut right = count;
                while left < right {
                    let mid = left + (right - left) / 2;
                    let offset = run_offset + mid * 4;
                    let start =
                        u16::from_le_bytes(self.run_zone[offset..offset + 2].try_into().unwrap());
                    let len = u16::from_le_bytes(
                        self.run_zone[offset + 2..offset + 4].try_into().unwrap(),
                    );
                    let end = start.saturating_add(len);
                    if index < start {
                        right = mid;
                    } else if index > end {
                        left = mid + 1;
                    } else {
                        return true;
                    }
                }
                false
            }
            ARRAY_CONTAINER_TYPE => {
                let mut left = 0;
                let mut right = count + 1;
                while left < right {
                    let mid = left + (right - left) / 2;
                    let offset = array_offset + mid * 2;
                    let val =
                        u16::from_le_bytes(self.array_zone[offset..offset + 2].try_into().unwrap());
                    match val.cmp(&index) {
                        Ordering::Less => left = mid + 1,
                        Ordering::Greater => right = mid,
                        Ordering::Equal => return true,
                    }
                }
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "std"))]
    use alloc::vec;

    use proptest::{prop_assert, prop_assert_eq, proptest};

    use super::*;

    fn create_comprehensive_bitmap() -> RoaringBitmap {
        let mut rb = RoaringBitmap::new();
        // 1. ArrayStore (Sparse data)
        rb.insert(1);
        rb.insert(100);

        // 2. BitmapStore (Dense, non-contiguous data: > 4096 elements to force BitmapStore)
        // Container key 3 (196608..262143)
        for i in (200_000..210_000).step_by(2) {
            rb.insert(i);
        }

        // 3. RunStore (Contiguous data, optimized)
        // Container key 6 & 7 (400_000..500_000)
        rb.insert_range(400_000..500_000);
        rb.optimize();

        rb
    }

    #[test]
    fn test_frozen_format_comprehensive() {
        let rb = create_comprehensive_bitmap();

        let size = rb.frozen_size_in_bytes();
        let mut buf = vec![0; size];
        rb.frozen_serialize_into(&mut buf);

        let view = FrozenRoaringBitmapView::new_unaligned(&buf).expect("Valid frozen bitmap");

        // Test API properties
        assert_eq!(view.len(), rb.len());
        assert_eq!(view.is_empty(), rb.is_empty());
        assert_eq!(view.min(), rb.min());
        assert_eq!(view.max(), rb.max());

        // Test ArrayStore
        assert!(view.contains(1));
        assert!(view.contains(100));
        assert!(!view.contains(2));

        // Test BitmapStore
        assert!(view.contains(200_000));
        assert!(view.contains(209_998));
        assert!(!view.contains(200_001));
        assert!(!view.contains(199_999));

        // Test RunStore
        assert!(view.contains(400_000));
        assert!(view.contains(450_000));
        assert!(view.contains(499_999));
        assert!(!view.contains(399_999));
        assert!(!view.contains(500_000));

        // Test To Owned Conversion
        let owned = view.to_roaring_bitmap();
        assert_eq!(owned, rb);
    }

    #[test]
    fn test_frozen_format_empty() {
        let rb = RoaringBitmap::new();
        let mut buf = vec![0; rb.frozen_size_in_bytes()];
        rb.frozen_serialize_into(&mut buf);

        let view = FrozenRoaringBitmapView::new_unaligned(&buf).unwrap();
        assert!(!view.contains(0));
        assert!(!view.contains(100));
        assert_eq!(view.len(), 0);
        assert!(view.is_empty());
        assert_eq!(view.min(), None);
        assert_eq!(view.max(), None);

        let owned = view.to_roaring_bitmap();
        assert_eq!(owned, rb);
    }

    #[test]
    fn test_frozen_alignment() {
        let mut rb = RoaringBitmap::new();
        rb.insert(1);
        let size = rb.frozen_size_in_bytes();

        // Create a buffer with extra space to test alignment offsets
        let mut buf = vec![0; size + 64];

        // Find a 32-byte aligned offset inside the vector
        let aligned_offset = (32 - (buf.as_ptr() as usize % 32)) % 32;
        let target_slice = &mut buf[aligned_offset..aligned_offset + size];
        rb.frozen_serialize_into(target_slice);

        // 1. Test perfectly aligned slice
        assert!(FrozenRoaringBitmapView::new(&buf[aligned_offset..aligned_offset + size]).is_some());

        // 2. Test unaligned slice (offset by 1)
        let unaligned_offset = aligned_offset + 1;
        let target_slice = &mut buf[unaligned_offset..unaligned_offset + size];
        rb.frozen_serialize_into(target_slice);

        // `new` must reject it for not being 32-byte aligned
        assert!(
            FrozenRoaringBitmapView::new(&buf[unaligned_offset..unaligned_offset + size]).is_none()
        );
        // `new_unaligned` must still successfully parse it
        assert!(FrozenRoaringBitmapView::new_unaligned(
            &buf[unaligned_offset..unaligned_offset + size]
        )
        .is_some());
    }

    #[test]
    fn test_frozen_malformed_and_truncated() {
        let rb = create_comprehensive_bitmap();
        let size = rb.frozen_size_in_bytes();
        let mut buf = vec![0; size];
        rb.frozen_serialize_into(&mut buf);

        // 1. Too small (less than a 4-byte header)
        assert!(FrozenRoaringBitmapView::new_unaligned(&[]).is_none());
        assert!(FrozenRoaringBitmapView::new_unaligned(&[0, 1, 2]).is_none());

        // 2. Wrong cookie
        let mut bad_cookie = buf.clone();
        // Change this to target the least significant byte of the 4-byte header
        let cookie_idx = bad_cookie.len() - 4;
        bad_cookie[cookie_idx] = 0; // Corrupt the FROZEN_COOKIE
        assert!(FrozenRoaringBitmapView::new_unaligned(&bad_cookie).is_none());

        // 3. Truncated data (valid header, but missing container bodies)
        assert!(FrozenRoaringBitmapView::new_unaligned(&buf[..size - 1]).is_none());
        assert!(FrozenRoaringBitmapView::new_unaligned(&buf[..size / 2]).is_none());
    }

    #[test]
    fn test_frozen_extreme_values() {
        let mut rb = RoaringBitmap::new();
        rb.insert(0);
        rb.insert(u32::MAX);

        let mut buf = vec![0; rb.frozen_size_in_bytes()];
        rb.frozen_serialize_into(&mut buf);

        let view = FrozenRoaringBitmapView::new_unaligned(&buf).unwrap();
        assert_eq!(view.len(), 2);
        assert_eq!(view.min(), Some(0));
        assert_eq!(view.max(), Some(u32::MAX));
        assert!(view.contains(0));
        assert!(view.contains(u32::MAX));
        assert!(!view.contains(1));
        assert!(!view.contains(u32::MAX - 1));
    }

    #[test]
    fn test_frozen_full_container() {
        let mut rb = RoaringBitmap::new();

        // Full container (key 0)
        rb.insert_range(0..=0xFFFF);

        // Array container (key 1)
        rb.insert(0x10000);

        // Run container (key 2)
        rb.insert_range(0x20000..=0x200FF);
        rb.optimize();

        let mut buf = vec![0; rb.frozen_size_in_bytes()];
        rb.frozen_serialize_into(&mut buf);

        let view = FrozenRoaringBitmapView::new_unaligned(&buf).unwrap();
        assert_eq!(view.len(), 65536 + 1 + 256);
        assert!(view.contains(0));
        assert!(view.contains(0xFFFF));
        assert!(view.contains(0x10000));
        assert!(view.contains(0x200FF));
        assert!(!view.contains(0x10001));
        assert!(!view.contains(0x20100));
    }

    proptest! {
        #[test]
        fn proptest_frozen_equivalence(bitmap in RoaringBitmap::arbitrary()) {
            let mut buf = vec![0; bitmap.frozen_size_in_bytes()];
            bitmap.frozen_serialize_into(&mut buf);

            let view = FrozenRoaringBitmapView::new_unaligned(&buf)
                .expect("Failed to initialize frozen view from valid serialized buffer");

            prop_assert_eq!(view.is_empty(), bitmap.is_empty());
            prop_assert_eq!(view.len(), bitmap.len());
            prop_assert_eq!(view.min(), bitmap.min());
            prop_assert_eq!(view.max(), bitmap.max());

            // Spot-check random points ensuring `contains` aligns completely
            if let Some(min) = bitmap.min() {
                prop_assert!(view.contains(min));
            }
            if let Some(max) = bitmap.max() {
                prop_assert!(view.contains(max));
            }

            // Iterate over a subset of actual items and ensure they are contained
            for val in bitmap.iter().take(100) {
                prop_assert!(view.contains(val));
            }
        }
    }
}
