//! ChaCha20-Poly1305 under the mesh key.
//!
//! Two modes, and the difference between them is the whole reason this file is
//! more than a wrapper. From `wire-format.md`:
//!
//! | `flags` bit 0 | associated data     | plaintext |
//! |---|---|---|
//! | 1 — encrypted  | header              | payload   |
//! | 0 — auth only  | header ‖ payload    | empty     |
//!
//! The encrypted mode is one call into `chacha20poly1305`. The
//! authenticated-only mode is not, because its associated data spans two slices
//! the caller holds separately and the AEAD API takes one contiguous `&[u8]`.
//! Joining them would need an allocation or a fixed buffer, and neither is
//! available: there is no allocator on a C3, and the maximum datagram size is
//! still an open question in the spec, so a fixed buffer would bake an
//! unresolved decision into an implementation.
//!
//! So the authenticated-only tag is streamed, following RFC 8439 §2.8 with an
//! empty ciphertext. That is composition, not invention — the primitives are
//! the same vetted `chacha20` and `poly1305` crates the AEAD itself is built
//! from — but it is still the kind of code that is wrong silently, so it is
//! tested against the library's own output for the equivalent concatenated
//! input over a sweep of lengths, including every length around the block and
//! padding boundaries.

use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha20;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;
use poly1305::universal_hash::UniversalHash;
use poly1305::{Block, Poly1305};

use lumen_proto::crypto::{Aead, NONCE_LEN};
use lumen_proto::header::TAG_LEN;

use crate::NotAuthentic;

/// Bytes of a mesh key.
pub const KEY_LEN: usize = 32;

/// The mesh key, and the AEAD built from it.
///
/// One per mesh. Holding the constructed cipher rather than the raw key means
/// the key schedule is set up once instead of per datagram, which at 60 Hz
/// across a mesh is not nothing.
#[derive(Clone)]
pub struct MeshKey {
    cipher: ChaCha20Poly1305,
    key: [u8; KEY_LEN],
}

impl MeshKey {
    pub fn new(key: &[u8; KEY_LEN]) -> MeshKey {
        MeshKey {
            cipher: ChaCha20Poly1305::new(key.into()),
            key: *key,
        }
    }

    /// The RFC 8439 tag over `header ‖ payload` as associated data, with an
    /// empty ciphertext.
    fn auth_only_tag(
        &self,
        nonce: &[u8; NONCE_LEN],
        associated_data: &[u8],
        payload: &[u8],
    ) -> [u8; TAG_LEN] {
        // The Poly1305 one-time key is the first 32 bytes of the ChaCha20
        // keystream at counter 0; the cipher proper starts at counter 1. That
        // split is what keeps the MAC key independent of anything encrypted.
        let mut cipher = ChaCha20::new((&self.key).into(), nonce.into());
        let mut otk = [0u8; 32];
        cipher.apply_keystream(&mut otk);

        let mut mac = Poly1305::new((&otk).into());
        let mut blocks = BlockFeeder::new();
        blocks.feed(&mut mac, associated_data);
        blocks.feed(&mut mac, payload);
        // One pad for the whole associated-data stream, not one per chunk —
        // feeding the two slices separately with a padded update each would
        // produce a different, and wrong, tag.
        blocks.pad(&mut mac);

        // pad16(ciphertext) is empty, then the two little-endian lengths.
        let aad_len = (associated_data.len() + payload.len()) as u64;
        let mut lengths = [0u8; 16];
        lengths[..8].copy_from_slice(&aad_len.to_le_bytes());
        // Second half stays zero: the ciphertext is empty.
        mac.update(core::slice::from_ref(Block::from_slice(&lengths)));

        let tag = mac.finalize();
        let mut out = [0u8; TAG_LEN];
        out.copy_from_slice(&tag);
        out
    }
}

/// Feeds a byte stream to Poly1305 as whole 16-byte blocks.
///
/// Exists because the associated data arrives as two slices whose lengths are
/// not multiples of the block size — the header is 24 bytes — and the boundary
/// between them is not a block boundary. Poly1305 must not see that seam.
struct BlockFeeder {
    partial: [u8; 16],
    used: usize,
}

impl BlockFeeder {
    fn new() -> BlockFeeder {
        BlockFeeder {
            partial: [0u8; 16],
            used: 0,
        }
    }

    fn feed(&mut self, mac: &mut Poly1305, mut data: &[u8]) {
        // An empty chunk must not disturb a partial block carried from the
        // previous one. Falling through would end at `self.used = 0` and throw
        // the carry away, which is a wrong tag rather than a crash.
        if data.is_empty() {
            return;
        }
        if self.used > 0 {
            let take = core::cmp::min(16 - self.used, data.len());
            self.partial[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];
            if self.used < 16 {
                // Still short of a block, so `data` is exhausted.
                return;
            }
            mac.update(core::slice::from_ref(Block::from_slice(&self.partial)));
            self.used = 0;
        }
        let whole = data.len() / 16 * 16;
        for chunk in data[..whole].chunks_exact(16) {
            mac.update(core::slice::from_ref(Block::from_slice(chunk)));
        }
        let rest = &data[whole..];
        self.partial[..rest.len()].copy_from_slice(rest);
        self.used = rest.len();
    }

    /// Flush the last partial block, zero-padded, as RFC 8439 requires.
    fn pad(&mut self, mac: &mut Poly1305) {
        if self.used > 0 {
            self.partial[self.used..].fill(0);
            mac.update(core::slice::from_ref(Block::from_slice(&self.partial)));
            self.used = 0;
        }
    }
}

impl Aead for MeshKey {
    type Error = NotAuthentic;

    fn seal(
        &self,
        nonce: &[u8; NONCE_LEN],
        associated_data: &[u8],
        in_out: &mut [u8],
        encrypt: bool,
    ) -> Result<[u8; TAG_LEN], NotAuthentic> {
        if !encrypt {
            return Ok(self.auth_only_tag(nonce, associated_data, in_out));
        }
        let tag = self
            .cipher
            .encrypt_in_place_detached(nonce.into(), associated_data, in_out)
            .map_err(|_| NotAuthentic)?;
        let mut out = [0u8; TAG_LEN];
        out.copy_from_slice(&tag);
        Ok(out)
    }

    fn open(
        &self,
        nonce: &[u8; NONCE_LEN],
        associated_data: &[u8],
        in_out: &mut [u8],
        tag: &[u8; TAG_LEN],
        encrypt: bool,
    ) -> Result<(), NotAuthentic> {
        if !encrypt {
            // Constant-time comparison: a byte-at-a-time `==` on a tag leaks
            // how many bytes matched, which is enough to forge one.
            let computed = self.auth_only_tag(nonce, associated_data, in_out);
            return if ct_eq(&computed, tag) {
                Ok(())
            } else {
                Err(NotAuthentic)
            };
        }
        // `decrypt_in_place_detached` verifies before it writes, so a failure
        // leaves no plaintext behind.
        self.cipher
            .decrypt_in_place_detached(nonce.into(), associated_data, in_out, tag.into())
            .map_err(|_| NotAuthentic)
    }
}

/// Compare two tags without an early exit.
fn ct_eq(a: &[u8; TAG_LEN], b: &[u8; TAG_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..TAG_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
#[path = "mesh_tests.rs"]
mod tests;
