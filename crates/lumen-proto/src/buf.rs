//! Bounds-checked little-endian cursors.
//!
//! Every field in the wire format is read through [`Reader`] and written through
//! [`Writer`], so there is exactly one place where a length check can be
//! forgotten — and it is covered by tests rather than by review.
//!
//! Both borrow their buffer. Nothing here allocates, which is what lets the same
//! codec run on an ESP32 and in the simulator.

use crate::error::{DecodeError, EncodeError};
use crate::Uuid;

type DResult<T> = Result<T, DecodeError>;
type EResult<T> = Result<T, EncodeError>;

/// A cursor that reads little-endian fields out of a byte slice.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Wrap a slice. The cursor starts at the beginning.
    pub const fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// True when every byte has been consumed.
    pub const fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// How far the cursor has advanced.
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// The bytes not yet consumed, without consuming them.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    fn take(&mut self, n: usize) -> DResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated);
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Consume `n` bytes.
    pub fn bytes(&mut self, n: usize) -> DResult<&'a [u8]> {
        self.take(n)
    }

    /// Consume and discard `n` bytes. Used for reserved fields, which are
    /// ignored on receive and never rejected.
    pub fn skip(&mut self, n: usize) -> DResult<()> {
        self.take(n).map(|_| ())
    }

    pub fn u8(&mut self) -> DResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> DResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> DResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> DResult<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn i32(&mut self) -> DResult<i32> {
        Ok(self.u32()? as i32)
    }

    /// A `q16`: Q16.16 fixed point carried in an `i32`.
    pub fn q16(&mut self) -> DResult<i32> {
        self.i32()
    }

    pub fn uuid(&mut self) -> DResult<Uuid> {
        let b = self.take(16)?;
        let mut out = [0u8; 16];
        out.copy_from_slice(b);
        Ok(Uuid(out))
    }

    /// A `str`: `u8` length followed by that many UTF-8 bytes.
    pub fn str(&mut self) -> DResult<&'a str> {
        let len = self.u8()? as usize;
        let raw = self.take(len)?;
        core::str::from_utf8(raw).map_err(|_| DecodeError::BadUtf8)
    }

    /// A `blob`: `u16` length followed by that many bytes.
    pub fn blob(&mut self) -> DResult<&'a [u8]> {
        let len = self.u16()? as usize;
        self.take(len)
    }

    /// A fixed-size array field, such as a 32-byte hash or a 64-byte signature.
    pub fn array<const N: usize>(&mut self) -> DResult<[u8; N]> {
        let b = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(b);
        Ok(out)
    }
}

/// A cursor that writes little-endian fields into a mutable byte slice.
#[derive(Debug)]
pub struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    /// Wrap a mutable slice. The cursor starts at the beginning.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Writer { buf, pos: 0 }
    }

    /// Bytes written so far.
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// Space left in the destination.
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Finish, returning the bytes written.
    pub fn into_written(self) -> &'a mut [u8] {
        &mut self.buf[..self.pos]
    }

    fn put(&mut self, src: &[u8]) -> EResult<()> {
        if self.remaining() < src.len() {
            return Err(EncodeError::BufferTooSmall {
                needed: src.len(),
                available: self.remaining(),
            });
        }
        self.buf[self.pos..self.pos + src.len()].copy_from_slice(src);
        self.pos += src.len();
        Ok(())
    }

    pub fn bytes(&mut self, src: &[u8]) -> EResult<()> {
        self.put(src)
    }

    /// Write `n` zero bytes. Reserved fields are zero on send.
    pub fn zeros(&mut self, n: usize) -> EResult<()> {
        if self.remaining() < n {
            return Err(EncodeError::BufferTooSmall {
                needed: n,
                available: self.remaining(),
            });
        }
        for i in 0..n {
            self.buf[self.pos + i] = 0;
        }
        self.pos += n;
        Ok(())
    }

    pub fn u8(&mut self, v: u8) -> EResult<()> {
        self.put(&[v])
    }

    pub fn u16(&mut self, v: u16) -> EResult<()> {
        self.put(&v.to_le_bytes())
    }

    pub fn u32(&mut self, v: u32) -> EResult<()> {
        self.put(&v.to_le_bytes())
    }

    pub fn u64(&mut self, v: u64) -> EResult<()> {
        self.put(&v.to_le_bytes())
    }

    pub fn i32(&mut self, v: i32) -> EResult<()> {
        self.put(&v.to_le_bytes())
    }

    /// A `q16`: Q16.16 fixed point carried in an `i32`.
    pub fn q16(&mut self, v: i32) -> EResult<()> {
        self.i32(v)
    }

    pub fn uuid(&mut self, v: &Uuid) -> EResult<()> {
        self.put(&v.0)
    }

    /// A `str`. Rejects anything over the 255-byte wire limit rather than
    /// truncating — a silently shortened name is worse than a failed send.
    pub fn str(&mut self, v: &str) -> EResult<()> {
        let bytes = v.as_bytes();
        if bytes.len() > u8::MAX as usize {
            return Err(EncodeError::StringTooLong { len: bytes.len() });
        }
        self.u8(bytes.len() as u8)?;
        self.put(bytes)
    }

    /// A `blob`. Rejects anything over the 65535-byte wire limit.
    pub fn blob(&mut self, v: &[u8]) -> EResult<()> {
        if v.len() > u16::MAX as usize {
            return Err(EncodeError::BlobTooLong { len: v.len() });
        }
        self.u16(v.len() as u16)?;
        self.put(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_scalar_little_endian() {
        let bytes = [
            0x01, // u8
            0x02, 0x01, // u16 = 0x0102
            0x04, 0x03, 0x02, 0x01, // u32 = 0x01020304
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // u64
        ];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 1);
        assert_eq!(r.u16().unwrap(), 0x0102);
        assert_eq!(r.u32().unwrap(), 0x0102_0304);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert!(r.is_empty());
        assert_eq!(r.position(), bytes.len());
    }

    #[test]
    fn q16_round_trips_through_negative_values() {
        let mut buf = [0u8; 4];
        let mut w = Writer::new(&mut buf);
        w.q16(-65536).unwrap(); // -1.0
        let mut r = Reader::new(&buf);
        assert_eq!(r.q16().unwrap(), -65536);
    }

    #[test]
    fn every_reader_method_reports_truncation() {
        let empty: [u8; 0] = [];
        assert_eq!(Reader::new(&empty).u8(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).u16(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).u32(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).u64(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).i32(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).q16(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).uuid(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).str(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).blob(), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).skip(1), Err(DecodeError::Truncated));
        assert_eq!(Reader::new(&empty).bytes(1), Err(DecodeError::Truncated));
        assert_eq!(
            Reader::new(&empty).array::<4>(),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn a_length_prefix_longer_than_the_buffer_is_truncation_not_a_panic() {
        // The hostile case: a well-formed length header promising bytes that are
        // not there. It must not index out of bounds.
        let bytes = [0x05, b'a', b'b'];
        assert_eq!(Reader::new(&bytes).str(), Err(DecodeError::Truncated));

        let blob = [0xff, 0xff, 0x00];
        assert_eq!(Reader::new(&blob).blob(), Err(DecodeError::Truncated));
    }

    #[test]
    fn rejects_invalid_utf8_in_a_str() {
        let bytes = [0x02, 0xff, 0xfe];
        assert_eq!(Reader::new(&bytes).str(), Err(DecodeError::BadUtf8));
    }

    #[test]
    fn str_and_blob_round_trip() {
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        w.str("kitchen").unwrap();
        w.blob(&[1, 2, 3]).unwrap();
        let n = w.position();

        let mut r = Reader::new(&buf[..n]);
        assert_eq!(r.str().unwrap(), "kitchen");
        assert_eq!(r.blob().unwrap(), &[1, 2, 3]);
        assert!(r.is_empty());
    }

    #[test]
    fn empty_str_and_blob_are_legal() {
        let mut buf = [0u8; 8];
        let mut w = Writer::new(&mut buf);
        w.str("").unwrap();
        w.blob(&[]).unwrap();
        let n = w.position();
        let mut r = Reader::new(&buf[..n]);
        assert_eq!(r.str().unwrap(), "");
        assert_eq!(r.blob().unwrap(), &[] as &[u8]);
    }

    #[test]
    fn uuid_round_trips() {
        let id = Uuid([9u8; 16]);
        let mut buf = [0u8; 16];
        Writer::new(&mut buf).uuid(&id).unwrap();
        assert_eq!(Reader::new(&buf).uuid().unwrap(), id);
    }

    #[test]
    fn array_reads_a_fixed_size_field() {
        let bytes = [7u8; 32];
        let got: [u8; 32] = Reader::new(&bytes).array().unwrap();
        assert_eq!(got, bytes);
    }

    #[test]
    fn rest_and_skip_agree() {
        let bytes = [1, 2, 3, 4];
        let mut r = Reader::new(&bytes);
        r.skip(2).unwrap();
        assert_eq!(r.rest(), &[3, 4]);
        assert_eq!(r.remaining(), 2);
    }

    #[test]
    fn writer_refuses_to_overflow_its_buffer() {
        let mut buf = [0u8; 2];
        let mut w = Writer::new(&mut buf);
        w.u8(1).unwrap();
        assert_eq!(
            w.u32(0),
            Err(EncodeError::BufferTooSmall {
                needed: 4,
                available: 1
            })
        );
        assert_eq!(
            w.zeros(9),
            Err(EncodeError::BufferTooSmall {
                needed: 9,
                available: 1
            })
        );
    }

    #[test]
    fn writer_rejects_oversized_str_and_blob() {
        let long = [b'x'; 300];
        let text = core::str::from_utf8(&long).unwrap();
        let mut buf = [0u8; 512];
        let mut w = Writer::new(&mut buf);
        assert_eq!(w.str(text), Err(EncodeError::StringTooLong { len: 300 }));

        // A blob one byte over the u16 limit. Allocating 64 KiB on the stack in a
        // test is fine; the codec itself never does.
        let mut big = [0u8; 65536];
        big[0] = 1;
        let mut sink = [0u8; 8];
        let mut w2 = Writer::new(&mut sink);
        assert_eq!(w2.blob(&big), Err(EncodeError::BlobTooLong { len: 65536 }));
    }

    #[test]
    fn zeros_writes_zeroes_and_into_written_returns_only_them() {
        let mut buf = [0xffu8; 8];
        let mut w = Writer::new(&mut buf);
        w.zeros(3).unwrap();
        assert_eq!(w.remaining(), 5);
        assert_eq!(w.into_written(), &[0, 0, 0]);
    }

    #[test]
    fn writer_bytes_copies_verbatim() {
        let mut buf = [0u8; 4];
        let mut w = Writer::new(&mut buf);
        w.bytes(&[1, 2, 3]).unwrap();
        assert_eq!(w.into_written(), &[1, 2, 3]);
    }

    #[test]
    fn encode_error_converts_from_decode_error() {
        let e: EncodeError = DecodeError::Truncated.into();
        assert_eq!(e, EncodeError::Invalid(DecodeError::Truncated));
    }
}
