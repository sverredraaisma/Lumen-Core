//! Putting a fragmented message back together.
//!
//! A message too large for one datagram is split across several, each carrying
//! [`Flags::FRAGMENT`], the first carrying [`Flags::FIRST_FRAGMENT`] and the last
//! carrying [`Flags::LAST_FRAGMENT`]. Fragments of one message are **consecutive
//! `sequence` values from the same sender**, so no fragment index or message id
//! is needed: the header already carries everything reassembly requires, and an
//! index would cost every datagram in the system four bytes to serve the rare
//! one.
//!
//! Sans-IO and allocation-free, like the rest of this crate. One of these per
//! sender, held by the caller, exactly as [`crate::replay::ReplayWindow`] is.
//!
//! **Nothing in the mesh fragments yet**, and that is why no device holds one of
//! these. The two messages that will are a sim snapshot, which waits on `sim`
//! being implemented in the compiler, and a state record, which travels over a
//! reliable transport and does not fragment on the wire at all. This is here
//! ahead of them for the same reason [`crate::replay`] is here ahead of
//! encryption: the wire format specifies it, so the implementation of the wire
//! format should carry it, and a receiver that meets a fragment today should
//! discard it correctly rather than misparse it.
//!
//! # What it refuses to do
//!
//! **It never waits.** A missing fragment discards the whole message rather than
//! holding what arrived in the hope of a retransmission. The two things that
//! fragment are a sim snapshot, which is replaced sixty times a second and would
//! be superseded before a retransmission could arrive, and a state record, which
//! travels over a reliable transport and does not fragment on the wire at all.
//!
//! **It never grows.** At most one incomplete message is held per sender, and a
//! fragment that does not continue the one in progress replaces it. A sender
//! that interleaves two fragmented messages is not conforming, and a receiver
//! must not spend memory finding that out.
//!
//! # Why the first fragment is marked
//!
//! Without [`Flags::FIRST_FRAGMENT`] this is unsound, and subtly: a receiver that
//! misses the opening fragment sees a fragment that does not continue anything,
//! starts a new message with it, and on the last fragment delivers a **truncated
//! message as if it were complete**. Nothing downstream can tell. The flag costs
//! a reserved bit and closes it — missing the start now means the message is
//! never begun.

use crate::header::Flags;

/// What [`Reassembler::accept`] decided about a datagram.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reassembly {
    /// Not a fragment. The caller should handle the payload as it stands; this
    /// reassembler has not touched it.
    ///
    /// An unfragmented datagram arriving mid-message also **abandons** that
    /// message, because fragments are consecutive and this one is not one of
    /// them.
    Whole,
    /// A fragment was stored and the message is not complete yet.
    Held,
    /// The message is complete. Read it with [`Reassembler::message`].
    Complete,
    /// The datagram was discarded, and why.
    Dropped(Dropped),
}

/// Why a fragment was thrown away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dropped {
    /// A fragment arrived that neither starts a message nor continues the one in
    /// progress, so one or more fragments were lost.
    ///
    /// Whatever was held is abandoned: it can only be a prefix of a message
    /// whose middle is missing.
    Gap,
    /// The message is larger than this receiver's buffer.
    ///
    /// Reported rather than silently truncated. A device that cannot hold a
    /// sender's messages should say so, because the alternative is a decoder
    /// downstream reporting a malformed payload and the real cause never being
    /// visible.
    TooLarge,
}

/// Reassembly state for one sender.
///
/// `N` is the largest message this receiver will accept, and is the caller's
/// choice because it is a memory budget rather than a protocol constant. A
/// device holding one of these per peer is spending `N` bytes per peer, which on
/// a small chip is the number that matters.
#[derive(Clone, Copy, Debug)]
pub struct Reassembler<const N: usize> {
    buf: [u8; N],
    len: usize,
    /// The `sequence` the next fragment must carry.
    next_sequence: u32,
    in_progress: bool,
}

impl<const N: usize> Default for Reassembler<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Reassembler<N> {
    pub const fn new() -> Self {
        Reassembler {
            buf: [0; N],
            len: 0,
            next_sequence: 0,
            in_progress: false,
        }
    }

    /// The reassembled message, valid after [`Reassembly::Complete`].
    ///
    /// Empty at any other time. Kept as a separate call rather than returned in
    /// the verdict so `accept` can borrow `self` mutably without lending the
    /// buffer out at the same time.
    pub fn message(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Whether a partially reassembled message is being held.
    pub fn is_in_progress(&self) -> bool {
        self.in_progress
    }

    /// Forget anything held.
    ///
    /// For a caller that has decided this sender's state is stale — it rebooted,
    /// or left the mesh — where continuing to hold a prefix would let it be
    /// completed by a fragment from a different message.
    pub fn reset(&mut self) {
        self.in_progress = false;
        self.len = 0;
    }

    /// Offer one datagram's payload.
    pub fn accept(&mut self, sequence: u32, flags: Flags, payload: &[u8]) -> Reassembly {
        if !flags.is_fragment() {
            // Fragments of a message are consecutive, so anything unfragmented
            // in the middle of one means the rest is never coming.
            self.reset();
            return Reassembly::Whole;
        }

        if flags.is_first_fragment() {
            // A new message replaces whatever was held. This is also the only
            // way a message can begin, which is what stops a receiver that
            // joined mid-message from delivering a truncated one.
            self.reset();
        } else if !self.in_progress || sequence != self.next_sequence {
            // Neither a start nor a continuation: fragments were lost. What is
            // held can only be a prefix of a message with a hole in it.
            self.reset();
            return Reassembly::Dropped(Dropped::Gap);
        }

        if self.len + payload.len() > N {
            self.reset();
            return Reassembly::Dropped(Dropped::TooLarge);
        }

        self.buf[self.len..self.len + payload.len()].copy_from_slice(payload);
        self.len += payload.len();
        self.in_progress = true;
        self.next_sequence = sequence.wrapping_add(1);

        if flags.is_last_fragment() {
            self.in_progress = false;
            Reassembly::Complete
        } else {
            Reassembly::Held
        }
    }
}

/// Split a message across datagrams.
///
/// Yields `(sequence, flags, chunk)` for each fragment, with the flags a sender
/// must use. A payload that already fits produces exactly one item carrying no
/// fragment flags at all, so a caller never needs to ask whether fragmenting is
/// necessary — it can send whatever comes out.
///
/// `chunk` is the largest payload one datagram may carry, which is
/// [`crate::header::MAX_PAYLOAD`] for an ordinary sender. It is a parameter
/// rather than that constant because a sender that has discovered a smaller path
/// MTU should use what it discovered.
pub fn fragment(payload: &[u8], sequence: u32, chunk: usize) -> Fragments<'_> {
    Fragments {
        payload,
        chunk: chunk.max(1),
        offset: 0,
        sequence,
        done: false,
    }
}

/// One fragment ready to send: the `sequence` it must carry, the `flags` it must
/// carry, and the slice of payload it holds.
pub type Fragment<'a> = (u32, Flags, &'a [u8]);

/// The iterator [`fragment`] returns.
#[derive(Clone, Debug)]
pub struct Fragments<'a> {
    payload: &'a [u8],
    chunk: usize,
    offset: usize,
    sequence: u32,
    done: bool,
}

impl<'a> Iterator for Fragments<'a> {
    type Item = Fragment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let fits = self.payload.len() <= self.chunk;
        let first = self.offset == 0;
        let end = (self.offset + self.chunk).min(self.payload.len());
        let chunk = &self.payload[self.offset..end];
        let last = end == self.payload.len();

        let flags = if fits {
            // Not fragmented at all. An empty payload lands here too, which is
            // why the iterator yields it once rather than yielding nothing: a
            // caller looping over this must still send the message.
            Flags::empty()
        } else {
            let mut f = Flags::empty().with(Flags::FRAGMENT);
            if first {
                f = f.with(Flags::FIRST_FRAGMENT);
            }
            if last {
                f = f.with(Flags::LAST_FRAGMENT);
            }
            f
        };

        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        self.offset = end;
        self.done = last;
        Some((sequence, flags, chunk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(bits: &[u8]) -> Flags {
        bits.iter().fold(Flags::empty(), |f, b| f.with(*b))
    }

    const FIRST: &[u8] = &[Flags::FRAGMENT, Flags::FIRST_FRAGMENT];
    const MIDDLE: &[u8] = &[Flags::FRAGMENT];
    const LAST: &[u8] = &[Flags::FRAGMENT, Flags::LAST_FRAGMENT];

    #[test]
    fn an_unfragmented_datagram_passes_straight_through() {
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(1, Flags::empty(), b"hello"), Reassembly::Whole);
        assert!(r.message().is_empty());
    }

    #[test]
    fn three_fragments_become_one_message() {
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(7, flags(FIRST), b"one "), Reassembly::Held);
        assert_eq!(r.accept(8, flags(MIDDLE), b"two "), Reassembly::Held);
        assert_eq!(r.accept(9, flags(LAST), b"three"), Reassembly::Complete);
        assert_eq!(r.message(), b"one two three");
        assert!(!r.is_in_progress());
    }

    #[test]
    fn a_message_of_one_fragment_is_legal_if_odd() {
        // First and last at once. Nothing should send this - it fits in a
        // datagram by definition - but a receiver that rejected it would be
        // refusing something the rules permit.
        let mut r = Reassembler::<64>::new();
        let both = flags(&[Flags::FRAGMENT, Flags::FIRST_FRAGMENT, Flags::LAST_FRAGMENT]);
        assert_eq!(r.accept(1, both, b"alone"), Reassembly::Complete);
        assert_eq!(r.message(), b"alone");
    }

    #[test]
    fn a_missing_middle_fragment_discards_the_message() {
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(1, flags(FIRST), b"one "), Reassembly::Held);
        // Fragment 2 was lost, so 3 does not continue anything.
        assert_eq!(
            r.accept(3, flags(LAST), b"three"),
            Reassembly::Dropped(Dropped::Gap)
        );
        assert!(!r.is_in_progress());
        assert!(r.message().is_empty());
    }

    #[test]
    fn missing_the_first_fragment_never_starts_a_message() {
        // The reason `FIRST_FRAGMENT` exists. Without it these two fragments
        // look like a whole message and would be delivered truncated, with
        // nothing downstream able to tell.
        let mut r = Reassembler::<64>::new();
        assert_eq!(
            r.accept(2, flags(MIDDLE), b"two "),
            Reassembly::Dropped(Dropped::Gap)
        );
        assert_eq!(
            r.accept(3, flags(LAST), b"three"),
            Reassembly::Dropped(Dropped::Gap)
        );
        assert!(r.message().is_empty());
    }

    #[test]
    fn a_new_message_replaces_one_left_incomplete() {
        // The bound on memory: at most one incomplete message per sender, so a
        // sender that abandons one cannot make a receiver hold it forever.
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(1, flags(FIRST), b"abandoned"), Reassembly::Held);
        assert_eq!(r.accept(5, flags(FIRST), b"fresh "), Reassembly::Held);
        assert_eq!(r.accept(6, flags(LAST), b"start"), Reassembly::Complete);
        assert_eq!(r.message(), b"fresh start");
    }

    #[test]
    fn an_unfragmented_datagram_abandons_a_message_in_progress() {
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(1, flags(FIRST), b"partial"), Reassembly::Held);
        assert_eq!(r.accept(2, Flags::empty(), b"whole"), Reassembly::Whole);
        assert!(!r.is_in_progress());
        // The abandoned prefix must not be completable by a later fragment.
        assert_eq!(
            r.accept(3, flags(LAST), b"tail"),
            Reassembly::Dropped(Dropped::Gap)
        );
    }

    #[test]
    fn a_message_larger_than_the_buffer_is_reported_not_truncated() {
        let mut r = Reassembler::<8>::new();
        assert_eq!(r.accept(1, flags(FIRST), b"12345"), Reassembly::Held);
        assert_eq!(
            r.accept(2, flags(LAST), b"67890"),
            Reassembly::Dropped(Dropped::TooLarge)
        );
        assert!(r.message().is_empty());
    }

    #[test]
    fn a_fragment_exactly_filling_the_buffer_is_accepted() {
        let mut r = Reassembler::<8>::new();
        assert_eq!(r.accept(1, flags(FIRST), b"1234"), Reassembly::Held);
        assert_eq!(r.accept(2, flags(LAST), b"5678"), Reassembly::Complete);
        assert_eq!(r.message(), b"12345678");
    }

    #[test]
    fn sequence_numbers_may_wrap() {
        // `sequence` is a u32 and a busy sender reaches the end of it. The
        // arithmetic has to wrap with it or a message spanning the wrap would be
        // rejected as a gap, once, unreproducibly, after weeks of uptime.
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(u32::MAX, flags(FIRST), b"end "), Reassembly::Held);
        assert_eq!(r.accept(0, flags(LAST), b"and start"), Reassembly::Complete);
        assert_eq!(r.message(), b"end and start");
    }

    #[test]
    fn a_reset_forgets_a_message_in_progress() {
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(1, flags(FIRST), b"stale"), Reassembly::Held);
        r.reset();
        assert!(!r.is_in_progress());
        assert_eq!(
            r.accept(2, flags(LAST), b"tail"),
            Reassembly::Dropped(Dropped::Gap)
        );
    }

    /// Collect an iterator into a fixed array, since this crate has no
    /// allocator even in its tests.
    fn collect<'a, const M: usize>(
        it: impl Iterator<Item = Fragment<'a>>,
    ) -> ([Fragment<'a>; M], usize) {
        let mut out = [(0, Flags::empty(), &b""[..]); M];
        let mut n = 0;
        for item in it {
            assert!(n < M, "more fragments than the test expected");
            out[n] = item;
            n += 1;
        }
        (out, n)
    }

    #[test]
    fn a_payload_that_fits_is_not_fragmented() {
        let (out, n) = collect::<4>(fragment(b"small", 10, 64));
        assert_eq!(n, 1);
        assert_eq!(out[0], (10, Flags::empty(), &b"small"[..]));
    }

    #[test]
    fn an_empty_payload_is_still_sent_once() {
        // Yielding nothing would silently drop a message whose payload is
        // legitimately empty, and the caller would have no way to notice.
        let (out, n) = collect::<4>(fragment(b"", 3, 64));
        assert_eq!(n, 1);
        assert_eq!(out[0].2, b"");
    }

    #[test]
    fn a_large_payload_splits_with_the_right_flags() {
        let (out, n) = collect::<8>(fragment(b"abcdefghij", 100, 4));
        assert_eq!(n, 3);
        assert_eq!(out[0], (100, flags(FIRST), &b"abcd"[..]));
        assert_eq!(out[1], (101, flags(MIDDLE), &b"efgh"[..]));
        assert_eq!(out[2], (102, flags(LAST), &b"ij"[..]));
    }

    #[test]
    fn what_is_split_reassembles_to_what_went_in() {
        // The property that matters, over sizes that land on and off a chunk
        // boundary and over a chunk size of one.
        let mut payload = [0u8; 100];
        for (n, b) in payload.iter_mut().enumerate() {
            *b = (n % 251) as u8;
        }

        for len in [1usize, 3, 4, 5, 16, 17, 100] {
            for chunk in [1usize, 2, 4, 7, 64] {
                let message = &payload[..len];
                let mut r = Reassembler::<256>::new();
                let mut last = Reassembly::Whole;
                for (seq, f, part) in fragment(message, 42, chunk) {
                    last = r.accept(seq, f, part);
                }
                match last {
                    Reassembly::Complete => assert_eq!(r.message(), message),
                    // A payload within one chunk is not fragmented, so the
                    // reassembler passes it through and holds nothing.
                    Reassembly::Whole => assert!(len <= chunk),
                    other => panic!("len {len} chunk {chunk}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_chunk_size_of_zero_does_not_hang() {
        // Nothing should ask for this, and a zero taken literally would divide
        // the payload into infinitely many empty fragments.
        let (_, n) = collect::<8>(fragment(b"ab", 0, 0).take(8));
        assert_eq!(n, 2);
    }

    #[test]
    fn an_empty_fragment_carries_nothing_and_breaks_nothing() {
        let mut r = Reassembler::<64>::new();
        assert_eq!(r.accept(1, flags(FIRST), b""), Reassembly::Held);
        assert_eq!(r.accept(2, flags(LAST), b"body"), Reassembly::Complete);
        assert_eq!(r.message(), b"body");
    }
}
