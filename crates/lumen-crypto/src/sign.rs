//! Ed25519 over programs and replicated records.
//!
//! Separate from the mesh key because the two answer different questions. The
//! mesh key says *this came from something inside the mesh*; a signature says
//! *this came from someone authorised to change what the mesh does*. A device
//! that conflated them would accept a program from any paired device.

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

use lumen_proto::crypto::{Signer, Verifier, PUBLIC_KEY_LEN, SIGNATURE_LEN};

use crate::NotAuthentic;

/// Bytes of an Ed25519 secret scalar seed.
pub type SecretKeyBytes = [u8; 32];

/// Verifies signatures. Holds nothing: the public key arrives per call, because
/// a device checks programs against whichever controller key it was paired
/// with, and records against their author's.
#[derive(Clone, Copy, Default, Debug)]
pub struct Ed25519Verifier;

impl Verifier for Ed25519Verifier {
    fn verify(
        &self,
        public_key: &[u8; PUBLIC_KEY_LEN],
        message: &[u8],
        signature: &[u8; SIGNATURE_LEN],
    ) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(public_key) else {
            // Not a point on the curve. Not a distinguishable condition from
            // "the signature is wrong" as far as any caller is concerned.
            return false;
        };
        let sig = Signature::from_bytes(signature);
        // `verify_strict` rejects small-order and non-canonical public keys,
        // which plain `verify` accepts. Those are the shapes that let one
        // signature verify under more than one key — malleability that matters
        // here, because a record's author is part of what is being asserted.
        key.verify_strict(message, &sig).is_ok()
    }
}

/// Signs with one key. A controller holds one of these; a device holds one for
/// its own identity and signs nothing else with it.
///
/// There is no `Debug`, no `Clone` and no accessor for the secret bytes. The
/// key goes in once and only signatures come out.
pub struct Ed25519Signer {
    key: SigningKey,
}

impl Ed25519Signer {
    /// Build from a 32-byte seed.
    pub fn from_seed(seed: &SecretKeyBytes) -> Ed25519Signer {
        Ed25519Signer {
            key: SigningKey::from_bytes(seed),
        }
    }

    /// The matching public key, to publish.
    pub fn public_key(&self) -> [u8; PUBLIC_KEY_LEN] {
        self.key.verifying_key().to_bytes()
    }
}

impl Signer for Ed25519Signer {
    type Error = NotAuthentic;

    fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], NotAuthentic> {
        Ok(self.key.sign(message).to_bytes())
    }
}

/// Verify with the same code path a device would use.
///
/// Deliberately not a method on [`Ed25519Signer`]: a signer that could also
/// verify invites a test that signs and verifies with one object and proves
/// nothing about whether the public key it publishes is the right one.
impl Ed25519Signer {
    #[cfg(test)]
    fn round_trips(&self, message: &[u8]) -> bool {
        let sig = self.sign(message).expect("signing cannot fail");
        Ed25519Verifier.verify(&self.public_key(), message, &sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> Ed25519Signer {
        Ed25519Signer::from_seed(&[7u8; 32])
    }

    #[test]
    fn a_signature_verifies_under_the_published_public_key() {
        assert!(signer().round_trips(b"a program"));
    }

    #[test]
    fn rfc_8032_test_vector_1() {
        // The published vector, so this is checked against the standard rather
        // than against itself. RFC 8032 §7.1, the empty message.
        let seed = hex_literal::hex!(
            "9d61b19deffd5a60ba844af492ec2cc4"
            "4449c5697b326919703bac031cae7f60"
        );
        let public = hex_literal::hex!(
            "d75a980182b10ab7d54bfed3c964073a"
            "0ee172f3daa62325af021a68f707511a"
        );
        let expected = hex_literal::hex!(
            "e5564300c360ac729086e2cc806e828a"
            "84877f1eb8e5d974d873e06522490155"
            "5fb8821590a33bacc61e39701cf9b46b"
            "d25bf5f0595bbe24655141438e7a100b"
        );
        let s = Ed25519Signer::from_seed(&seed);
        assert_eq!(s.public_key(), public, "public key derivation");
        assert_eq!(s.sign(b"").expect("sign"), expected, "signature");
        assert!(Ed25519Verifier.verify(&public, b"", &expected));
    }

    #[test]
    fn rfc_8032_test_vector_2() {
        let seed = hex_literal::hex!(
            "4ccd089b28ff96da9db6c346ec114e0f"
            "5b8a319f35aba624da8cf6ed4fb8a6fb"
        );
        let public = hex_literal::hex!(
            "3d4017c3e843895a92b70aa74d1b7ebc"
            "9c982ccf2ec4968cc0cd55f12af4660c"
        );
        let expected = hex_literal::hex!(
            "92a009a9f0d4cab8720e820b5f642540"
            "a2b27b5416503f8fb3762223ebdb69da"
            "085ac1e43e15996e458f3613d0f11d8c"
            "387b2eaeb4302aeeb00d291612bb0c00"
        );
        let s = Ed25519Signer::from_seed(&seed);
        assert_eq!(s.public_key(), public);
        assert_eq!(s.sign(&[0x72]).expect("sign"), expected);
        assert!(Ed25519Verifier.verify(&public, &[0x72], &expected));
    }

    #[test]
    fn a_tampered_message_does_not_verify() {
        let s = signer();
        let sig = s.sign(b"activate scene 4").expect("sign");
        assert!(!Ed25519Verifier.verify(&s.public_key(), b"activate scene 5", &sig));
    }

    #[test]
    fn another_key_does_not_verify() {
        let a = signer();
        let b = Ed25519Signer::from_seed(&[8u8; 32]);
        let sig = a.sign(b"a record").expect("sign");
        assert!(!Ed25519Verifier.verify(&b.public_key(), b"a record", &sig));
    }

    #[test]
    fn a_flipped_bit_anywhere_in_the_signature_is_rejected() {
        let s = signer();
        let msg = b"a program";
        let sig = s.sign(msg).expect("sign");
        let pk = s.public_key();
        for byte in 0..SIGNATURE_LEN {
            for bit in 0..8 {
                let mut bad = sig;
                bad[byte] ^= 1 << bit;
                assert!(
                    !Ed25519Verifier.verify(&pk, msg, &bad),
                    "signature with bit {bit} of byte {byte} flipped verified"
                );
            }
        }
    }

    #[test]
    fn a_public_key_that_is_not_a_curve_point_is_rejected_rather_than_panicking() {
        // Comes off the wire, so it is attacker-controlled. It must be a plain
        // `false`, not a panic and not a distinguishable error.
        let sig = signer().sign(b"x").expect("sign");
        assert!(!Ed25519Verifier.verify(&[0xFF; PUBLIC_KEY_LEN], b"x", &sig));
    }

    #[test]
    fn the_all_zero_public_key_is_rejected() {
        // A small-order point. `verify_strict` refuses these; plain `verify`
        // does not, and that difference is why this crate uses the strict one.
        let sig = signer().sign(b"x").expect("sign");
        assert!(!Ed25519Verifier.verify(&[0u8; PUBLIC_KEY_LEN], b"x", &sig));
    }

    #[test]
    fn signing_is_deterministic() {
        // Ed25519 derives its nonce from the key and message, so two signatures
        // over the same input are identical. Records are content-addressed by
        // their signature in places, and a randomised one would break that.
        let s = signer();
        assert_eq!(s.sign(b"same").unwrap(), s.sign(b"same").unwrap());
    }

    #[test]
    fn an_empty_message_is_signable() {
        assert!(signer().round_trips(b""));
    }
}
