//! Tests for the mesh AEAD.
//!
//! In their own file because they need the private `auth_only_tag` and the
//! constructed cipher: the central test compares the streamed tag against the
//! library's own output for the equivalent concatenated input, and an
//! integration test could only compare the implementation with itself.

use super::*;
use lumen_proto::crypto::{open_in_place, seal_in_place};
use lumen_proto::header::HEADER_LEN;
use std::vec::Vec;

fn key() -> MeshKey {
    MeshKey::new(&[0x42; KEY_LEN])
}

const NONCE: [u8; NONCE_LEN] = [9; NONCE_LEN];

/// The library's tag for the same inputs, with the associated data joined into
/// one slice. This is the oracle the streamed implementation is checked
/// against.
fn library_auth_only_tag(k: &MeshKey, aad: &[u8], payload: &[u8]) -> [u8; TAG_LEN] {
    let mut joined = Vec::new();
    joined.extend_from_slice(aad);
    joined.extend_from_slice(payload);
    let tag = k
        .cipher
        .encrypt_in_place_detached(&NONCE.into(), &joined, &mut [])
        .expect("an empty plaintext always seals");
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&tag);
    out
}

#[test]
fn the_streamed_tag_matches_the_library_at_every_length_around_a_block_boundary() {
    // The seam between the two slices is not a block boundary - the header is
    // 24 bytes - so the feeder carries a partial block across it. Every length
    // from empty to past two blocks, so no off-by-one in the carry, the padding
    // or the length encoding can survive.
    let k = key();
    for aad_len in 0..40usize {
        for payload_len in 0..40usize {
            let aad: Vec<u8> = (0..aad_len).map(|i| i as u8).collect();
            let payload: Vec<u8> = (0..payload_len).map(|i| (200usize - i) as u8).collect();
            assert_eq!(
                k.auth_only_tag(&NONCE, &aad, &payload),
                library_auth_only_tag(&k, &aad, &payload),
                "aad {aad_len}, payload {payload_len}"
            );
        }
    }
}

#[test]
fn the_streamed_tag_matches_the_library_for_a_realistic_datagram() {
    let k = key();
    let aad: Vec<u8> = (0..HEADER_LEN).map(|i| i as u8).collect();
    for payload_len in [0usize, 1, 15, 16, 17, 300, 1200] {
        let payload: Vec<u8> = (0..payload_len).map(|i| (i * 7) as u8).collect();
        assert_eq!(
            k.auth_only_tag(&NONCE, &aad, &payload),
            library_auth_only_tag(&k, &aad, &payload),
            "payload {payload_len}"
        );
    }
}

#[test]
fn rfc_8439_aead_test_vector() {
    // RFC 8439 section 2.8.2. Checks the encrypted path against the standard
    // rather than against itself, and proves the crate is wired to the
    // algorithm the wire format names.
    let k = MeshKey::new(&hex_literal::hex!(
        "808182838485868788898a8b8c8d8e8f"
        "909192939495969798999a9b9c9d9e9f"
    ));
    let nonce = hex_literal::hex!("070000004041424344454647");
    let aad = hex_literal::hex!("50515253c0c1c2c3c4c5c6c7");
    let mut buf = *b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";

    let tag = k.seal(&nonce, &aad, &mut buf, true).expect("seal");
    assert_eq!(
        buf[..16],
        hex_literal::hex!("d31a8d34648e60db7b86afbc53ef7ec2")[..],
        "ciphertext prefix"
    );
    assert_eq!(
        tag,
        hex_literal::hex!("1ae10b594f09e26a7e902ecbd0600691"),
        "tag"
    );

    k.open(&nonce, &aad, &mut buf, &tag, true).expect("open");
    assert_eq!(&buf[..7], b"Ladies ", "decrypt must restore the plaintext");
}

#[test]
fn an_encrypted_payload_round_trips_and_is_actually_encrypted() {
    let k = key();
    let aad = [1u8; HEADER_LEN];
    let plain = *b"a scene that is nobody elses business";
    let mut buf = plain;

    let tag = k.seal(&NONCE, &aad, &mut buf, true).expect("seal");
    assert_ne!(buf, plain, "encrypted mode must not leave the plaintext");

    k.open(&NONCE, &aad, &mut buf, &tag, true).expect("open");
    assert_eq!(buf, plain);
}

#[test]
fn an_authenticated_only_payload_is_left_in_the_clear() {
    // The whole point of the mode: the bytes on the wire are the plaintext, and
    // no cipher pass is spent on them.
    let k = key();
    let aad = [1u8; HEADER_LEN];
    let plain = *b"pixels are not secret";
    let mut buf = plain;

    let tag = k.seal(&NONCE, &aad, &mut buf, false).expect("seal");
    assert_eq!(buf, plain, "authenticated-only must not encrypt");

    k.open(&NONCE, &aad, &mut buf, &tag, false).expect("open");
    assert_eq!(buf, plain);
}

#[test]
fn a_tag_from_one_mode_never_verifies_in_the_other() {
    // The modes feed Poly1305 different byte streams for the same datagram, so
    // a downgrade cannot be forged by reinterpreting a tag. This is the property
    // the wire format leans on when it says bit 0 is authenticated.
    let k = key();
    let aad = [3u8; HEADER_LEN];
    let plain = *b"same bytes either way";

    let mut a = plain;
    let auth_tag = k.seal(&NONCE, &aad, &mut a, false).expect("seal");
    let mut b = plain;
    let enc_tag = k.seal(&NONCE, &aad, &mut b, true).expect("seal");
    assert_ne!(auth_tag, enc_tag, "the two modes must not share a tag");

    let mut probe = plain;
    assert!(k.open(&NONCE, &aad, &mut probe, &enc_tag, false).is_err());
    let mut probe = b;
    assert!(k.open(&NONCE, &aad, &mut probe, &auth_tag, true).is_err());
}

#[test]
fn tampering_with_the_payload_is_caught_in_both_modes() {
    let k = key();
    let aad = [5u8; HEADER_LEN];
    for encrypt in [true, false] {
        let mut buf = *b"twelve bytes";
        let tag = k.seal(&NONCE, &aad, &mut buf, encrypt).expect("seal");
        for i in 0..buf.len() {
            let mut bad = buf;
            bad[i] ^= 0x01;
            assert!(
                k.open(&NONCE, &aad, &mut bad, &tag, encrypt).is_err(),
                "encrypt={encrypt}, byte {i}"
            );
        }
    }
}

#[test]
fn tampering_with_the_associated_data_is_caught_in_both_modes() {
    let k = key();
    for encrypt in [true, false] {
        let aad = [5u8; HEADER_LEN];
        let mut buf = *b"payload!";
        let tag = k.seal(&NONCE, &aad, &mut buf, encrypt).expect("seal");
        for i in 0..HEADER_LEN {
            let mut bad_aad = aad;
            bad_aad[i] ^= 0x80;
            let mut probe = buf;
            assert!(
                k.open(&NONCE, &bad_aad, &mut probe, &tag, encrypt).is_err(),
                "encrypt={encrypt}, header byte {i}"
            );
        }
    }
}

#[test]
fn a_flipped_bit_anywhere_in_the_tag_is_rejected() {
    let k = key();
    let aad = [0u8; HEADER_LEN];
    for encrypt in [true, false] {
        let mut buf = *b"authentic";
        let tag = k.seal(&NONCE, &aad, &mut buf, encrypt).expect("seal");
        for byte in 0..TAG_LEN {
            for bit in 0..8 {
                let mut bad = tag;
                bad[byte] ^= 1 << bit;
                let mut probe = buf;
                assert!(
                    k.open(&NONCE, &aad, &mut probe, &bad, encrypt).is_err(),
                    "encrypt={encrypt}, bit {bit} of tag byte {byte}"
                );
            }
        }
    }
}

#[test]
fn a_different_nonce_does_not_verify() {
    let k = key();
    let aad = [0u8; HEADER_LEN];
    let mut buf = *b"replayed";
    let tag = k.seal(&NONCE, &aad, &mut buf, false).expect("seal");
    let mut other = NONCE;
    other[0] ^= 1;
    let mut probe = buf;
    assert!(k.open(&other, &aad, &mut probe, &tag, false).is_err());
}

#[test]
fn a_different_key_does_not_verify() {
    let aad = [0u8; HEADER_LEN];
    let mut buf = *b"someone elses mesh";
    let tag = key().seal(&NONCE, &aad, &mut buf, false).expect("seal");
    let other = MeshKey::new(&[0x43; KEY_LEN]);
    let mut probe = buf;
    assert!(other.open(&NONCE, &aad, &mut probe, &tag, false).is_err());
}

#[test]
fn an_empty_payload_is_sealable_in_both_modes() {
    let k = key();
    let aad = [2u8; HEADER_LEN];
    for encrypt in [true, false] {
        let mut empty: [u8; 0] = [];
        let tag = k.seal(&NONCE, &aad, &mut empty, encrypt).expect("seal");
        assert!(k.open(&NONCE, &aad, &mut empty, &tag, encrypt).is_ok());
    }
}

#[test]
fn a_failed_open_leaves_no_plaintext_behind() {
    // A receiver that wrote decrypted bytes and only then checked the tag would
    // hand an attacker a decryption oracle a byte at a time.
    let k = key();
    let aad = [0u8; HEADER_LEN];
    let mut buf = *b"secret enough";
    let tag = k.seal(&NONCE, &aad, &mut buf, true).expect("seal");
    let ciphertext = buf;

    let mut bad = tag;
    bad[0] ^= 1;
    assert!(k.open(&NONCE, &aad, &mut buf, &bad, true).is_err());
    assert_eq!(buf, ciphertext, "a failed open must not decrypt in place");
}

// ---- through the seam's own helpers -----------------------------------------

/// Build a datagram buffer the way `seal_in_place` expects: header, then
/// payload, then room for the tag.
fn datagram(encrypted: bool, payload: &[u8]) -> Vec<u8> {
    use lumen_proto::header::{Flags, Header};
    use lumen_proto::MsgType;
    let mut header = Header::new(MsgType::Tick, [0xAB, 0xCD], [1, 2, 3, 4], 7, 123_456);
    if encrypted {
        header.flags = Flags::empty().with(Flags::ENCRYPTED);
    }
    header.payload_len = payload.len() as u16;
    let mut buf = std::vec![0u8; HEADER_LEN + payload.len() + TAG_LEN];
    header
        .encode(&mut buf[..HEADER_LEN])
        .expect("encode header");
    buf[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
    buf
}

#[test]
fn a_datagram_round_trips_through_seal_and_open_in_both_modes() {
    let k = key();
    for encrypted in [true, false] {
        let payload = b"the payload as it will be sent";
        let mut buf = datagram(encrypted, payload);
        let sealed = seal_in_place(&k, &mut buf, payload.len(), 11).expect("seal_in_place");
        assert_eq!(sealed.len, buf.len());

        let (header, out) = open_in_place(&k, &mut buf, 11).expect("open_in_place");
        assert_eq!(header.sequence, 7);
        assert_eq!(out, payload, "encrypted={encrypted}");
    }
}

#[test]
fn the_boot_counter_is_part_of_the_nonce_end_to_end() {
    // A device that rebooted restarts its sequence at zero; without the boot
    // counter in the nonce that is nonce reuse under the same key. Opening with
    // the wrong counter must fail, or the field is decorative.
    let k = key();
    let payload = b"after a reboot";
    let mut buf = datagram(false, payload);
    seal_in_place(&k, &mut buf, payload.len(), 11).expect("seal");
    assert!(
        open_in_place(&k, &mut buf, 12).is_err(),
        "wrong boot counter"
    );
}
