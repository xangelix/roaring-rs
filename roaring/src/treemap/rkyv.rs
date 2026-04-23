//! Rkyv support for [`RoaringTreemap`].

use alloc::collections::BTreeMap;
use core::fmt;

use rkyv::{
    collections::btree_map::{ArchivedBTreeMap, BTreeMapResolver},
    rancor::{Fallible, Source},
    Archive, Deserialize, Place, Portable, Serialize,
};

use crate::{ArchivedRoaringBitmap, RoaringBitmap, RoaringTreemap};

#[derive(Portable)]
#[repr(transparent)]
#[cfg_attr(feature = "rkyv_bytecheck", derive(rkyv::bytecheck::CheckBytes))]
#[cfg_attr(feature = "rkyv_bytecheck", bytecheck(crate = rkyv::bytecheck))]
/// An archived representation of a [`RoaringTreemap`] that is optimized for zero-copy deserialization and read-only access.
pub struct ArchivedRoaringTreemap(
    pub  rkyv::collections::btree_map::ArchivedBTreeMap<
        rkyv::primitive::ArchivedU32,
        ArchivedRoaringBitmap,
    >,
);

impl ArchivedRoaringTreemap {
    /// Checks if the archived roaring treemap is empty.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of elements in the archived roaring treemap.
    #[must_use]
    #[inline]
    pub fn len(&self) -> u64 {
        self.0.values().map(ArchivedRoaringBitmap::len).sum()
    }

    /// Returns the minimum value in the archived roaring treemap, or `None` if it is empty.
    #[must_use]
    #[inline]
    pub fn min(&self) -> Option<u64> {
        self.0.iter().find(|(_, rb)| !rb.is_empty()).map(|(k, rb)| {
            let hi = k.to_native();
            let lo = rb.min().unwrap();
            (u64::from(hi) << 32) | u64::from(lo)
        })
    }

    /// Returns the maximum value in the archived roaring treemap, or `None` if it is empty.
    #[must_use]
    #[inline]
    pub fn max(&self) -> Option<u64> {
        self.0.iter().filter(|(_, rb)| !rb.is_empty()).last().map(|(k, rb)| {
            let hi = k.to_native();
            let lo = rb.max().unwrap();
            (u64::from(hi) << 32) | u64::from(lo)
        })
    }

    /// Checks if the archived roaring treemap contains the given value.
    #[must_use]
    #[inline]
    pub fn contains(&self, value: u64) -> bool {
        let hi = (value >> 32) as u32;
        let lo = value as u32;
        let hi_archived = rkyv::primitive::ArchivedU32::from_native(hi);
        self.0.get(&hi_archived).is_some_and(|rb| rb.contains(lo))
    }
}

impl fmt::Debug for ArchivedRoaringTreemap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ArchivedRoaringTreemap(<{} elements>)", self.0.len())
    }
}

impl PartialEq for ArchivedRoaringTreemap {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for ArchivedRoaringTreemap {}

impl Archive for RoaringTreemap {
    type Archived = ArchivedRoaringTreemap;
    type Resolver = BTreeMapResolver;

    #[inline]
    fn resolve(&self, resolver: Self::Resolver, out: Place<Self::Archived>) {
        rkyv::munge::munge!(let ArchivedRoaringTreemap(map) = out);
        ArchivedBTreeMap::resolve_from_len(self.map.len(), resolver, map);
    }
}

impl<S: Fallible + rkyv::ser::Allocator + rkyv::ser::Writer + ?Sized> Serialize<S>
    for RoaringTreemap
where
    S::Error: Source,
{
    fn serialize(&self, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        rkyv::collections::btree_map::ArchivedBTreeMap::<
            rkyv::primitive::ArchivedU32,
            ArchivedRoaringBitmap,
        >::serialize_from_ordered_iter::<
            alloc::collections::btree_map::Iter<'_, u32, RoaringBitmap>,
            &u32,
            &RoaringBitmap,
            u32,
            RoaringBitmap,
            S,
        >(self.map.iter(), serializer)
    }
}

impl<D: Fallible + ?Sized> Deserialize<RoaringTreemap, D> for ArchivedRoaringTreemap
where
    D::Error: Source,
{
    fn deserialize(&self, deserializer: &mut D) -> Result<RoaringTreemap, D::Error> {
        let mut map = BTreeMap::new();
        for (k, v) in self.0.iter() {
            map.insert(k.to_native(), v.deserialize(deserializer)?);
        }
        Ok(RoaringTreemap { map })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "std"))]
    use alloc::format;

    use rkyv::{deserialize, rancor::Error};

    use super::{ArchivedRoaringTreemap, RoaringTreemap};

    #[test]
    fn rkyv_roundtrip_empty() {
        let rt = RoaringTreemap::new();
        let bytes = rkyv::to_bytes::<Error>(&rt).unwrap();
        let archived = rkyv::access::<ArchivedRoaringTreemap, Error>(&bytes).unwrap();
        assert_eq!(archived.len(), 0);
        let deserialized: RoaringTreemap = deserialize::<RoaringTreemap, Error>(archived).unwrap();
        assert_eq!(deserialized, rt);
    }

    #[test]
    fn rkyv_roundtrip_basic() {
        let mut rt = RoaringTreemap::new();
        rt.insert(1);
        rt.insert(2);
        rt.insert(u32::MAX as u64 + 1);
        rt.insert(u32::MAX as u64 + 100_000);
        rt.insert_range((u32::MAX as u64 * 2)..=(u32::MAX as u64 * 2 + 100_000));

        let bytes = rkyv::to_bytes::<Error>(&rt).unwrap();
        let archived = rkyv::access::<ArchivedRoaringTreemap, Error>(&bytes).unwrap();

        assert_eq!(archived.len(), rt.len());
        assert_eq!(archived.is_empty(), rt.is_empty());
        assert_eq!(archived.min(), rt.min());
        assert_eq!(archived.max(), rt.max());

        assert!(archived.contains(1));
        assert!(archived.contains(u32::MAX as u64 + 100_000));
        assert!(!archived.contains(3));

        let deserialized: RoaringTreemap = deserialize::<RoaringTreemap, Error>(archived).unwrap();
        assert_eq!(deserialized, rt);
    }

    #[test]
    fn rkyv_archive_debug() {
        let mut rt = RoaringTreemap::new();
        rt.insert(1);
        let bytes = rkyv::to_bytes::<Error>(&rt).unwrap();
        let archived = rkyv::access::<ArchivedRoaringTreemap, Error>(&bytes).unwrap();
        let _ = format!("{archived:?}");
    }
}
