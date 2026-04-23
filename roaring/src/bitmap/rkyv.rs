//! Rkyv support for [`RoaringBitmap`].

use core::fmt;

use rkyv::{
    rancor::{fail, Fallible, Source},
    vec::{ArchivedVec, VecResolver},
    Archive, Deserialize, Place, Portable, Serialize,
};

use crate::{FrozenRoaringBitmapView, RoaringBitmap};

#[derive(Debug)]
/// An error type that is returned when an archived roaring bitmap is invalid and cannot be deserialized.
pub struct InvalidFrozenBitmap;

impl fmt::Display for InvalidFrozenBitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid frozen roaring bitmap")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InvalidFrozenBitmap {}

#[derive(Portable)]
#[repr(transparent)]
#[cfg_attr(feature = "rkyv_bytecheck", derive(rkyv::bytecheck::CheckBytes))]
#[cfg_attr(feature = "rkyv_bytecheck", bytecheck(crate = rkyv::bytecheck))]
#[cfg_attr(feature = "rkyv_bytecheck", bytecheck(verify))]
/// An archived representation of a [`RoaringBitmap`] that is optimized for zero-copy deserialization and read-only access.
pub struct ArchivedRoaringBitmap(ArchivedVec<u8>);

#[cfg(feature = "rkyv_bytecheck")]
mod verify {
    use super::{ArchivedRoaringBitmap, InvalidFrozenBitmap};
    use rkyv::bytecheck::Verify;
    use rkyv::rancor::{fail, Fallible};

    unsafe impl<C: Fallible + ?Sized> Verify<C> for ArchivedRoaringBitmap
    where
        C::Error: rkyv::rancor::Source,
    {
        fn verify(&self, _context: &mut C) -> Result<(), C::Error> {
            if crate::bitmap::frozen::FrozenRoaringBitmapView::new_unaligned(self.0.as_slice())
                .is_none()
            {
                fail!(InvalidFrozenBitmap);
            }
            Ok(())
        }
    }
}

impl ArchivedRoaringBitmap {
    /// Views this archived roaring bitmap representation over its bytes.
    #[must_use]
    #[inline]
    pub fn as_view(&self) -> Option<FrozenRoaringBitmapView<'_>> {
        FrozenRoaringBitmapView::new_unaligned(self.0.as_slice())
    }

    /// Checks if the archived roaring bitmap contains the given value.
    #[must_use]
    #[inline]
    pub fn contains(&self, value: u32) -> bool {
        self.as_view().is_some_and(|view| view.contains(value))
    }

    /// Checks if the archived roaring bitmap is empty.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_view().is_none_or(|view| view.is_empty())
    }

    /// Returns the number of elements in the archived roaring bitmap.
    #[must_use]
    #[inline]
    pub fn len(&self) -> u64 {
        self.as_view().map_or(0, |view| view.len())
    }

    /// Returns the minimum value in the archived roaring bitmap, or `None` if it is empty.
    #[must_use]
    #[inline]
    pub fn min(&self) -> Option<u32> {
        self.as_view().and_then(|view| view.min())
    }

    /// Returns the maximum value in the archived roaring bitmap, or `None` if it is empty.
    #[must_use]
    #[inline]
    pub fn max(&self) -> Option<u32> {
        self.as_view().and_then(|view| view.max())
    }
}

impl fmt::Debug for ArchivedRoaringBitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ArchivedRoaringBitmap(<{} bytes>)", self.0.len())
    }
}

impl PartialEq for ArchivedRoaringBitmap {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

impl Eq for ArchivedRoaringBitmap {}

impl Archive for RoaringBitmap {
    type Archived = ArchivedRoaringBitmap;
    type Resolver = VecResolver;

    #[inline]
    fn resolve(&self, resolver: Self::Resolver, out: Place<Self::Archived>) {
        rkyv::munge::munge!(let ArchivedRoaringBitmap(vec_out) = out);
        ArchivedVec::resolve_from_len(self.frozen_size_in_bytes(), resolver, vec_out);
    }
}

impl<S: Fallible + rkyv::ser::Writer + rkyv::ser::Allocator + ?Sized> Serialize<S> for RoaringBitmap
where
    S::Error: Source,
{
    fn serialize(&self, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        let size = self.frozen_size_in_bytes();
        let mut buf = vec![0; size];
        self.frozen_serialize_into(&mut buf);
        ArchivedVec::serialize_from_slice(&buf, serializer)
    }
}

impl<D: Fallible + ?Sized> Deserialize<RoaringBitmap, D> for ArchivedRoaringBitmap
where
    D::Error: Source,
{
    fn deserialize(&self, _deserializer: &mut D) -> Result<RoaringBitmap, D::Error> {
        if let Some(view) = self.as_view() {
            Ok(view.to_roaring_bitmap())
        } else {
            fail!(InvalidFrozenBitmap);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "std"))]
    use alloc::format;

    use rkyv::{deserialize, rancor::Error};

    use super::{ArchivedRoaringBitmap, RoaringBitmap};

    #[test]
    fn rkyv_roundtrip_empty() {
        let rb = RoaringBitmap::new();
        let bytes = rkyv::to_bytes::<Error>(&rb).unwrap();
        let archived = rkyv::access::<ArchivedRoaringBitmap, Error>(&bytes).unwrap();
        assert_eq!(archived.len(), 0);
        let deserialized: RoaringBitmap = deserialize::<RoaringBitmap, Error>(archived).unwrap();
        assert_eq!(deserialized, rb);
    }

    #[test]
    fn rkyv_roundtrip_basic() {
        let mut rb = RoaringBitmap::new();
        rb.insert(1);
        rb.insert(2);
        rb.insert(100_000);
        rb.insert_range(200_000..=300_000);

        let bytes = rkyv::to_bytes::<Error>(&rb).unwrap();
        let archived = rkyv::access::<ArchivedRoaringBitmap, Error>(&bytes).unwrap();

        assert_eq!(archived.len(), rb.len());
        assert_eq!(archived.is_empty(), rb.is_empty());
        assert_eq!(archived.min(), rb.min());
        assert_eq!(archived.max(), rb.max());

        assert!(archived.contains(1));
        assert!(archived.contains(100_000));
        assert!(archived.contains(250_000));
        assert!(!archived.contains(3));

        let deserialized: RoaringBitmap = deserialize::<RoaringBitmap, Error>(archived).unwrap();
        assert_eq!(deserialized, rb);
    }

    #[test]
    fn rkyv_archive_debug() {
        let mut rb = RoaringBitmap::new();
        rb.insert(1);
        let bytes = rkyv::to_bytes::<Error>(&rb).unwrap();
        let archived = rkyv::access::<ArchivedRoaringBitmap, Error>(&bytes).unwrap();
        let _ = format!("{archived:?}");
    }
}
