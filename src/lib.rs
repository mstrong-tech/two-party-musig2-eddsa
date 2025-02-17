//!    Two-party implementation of the Musig2 protocol for multi-signatures of EdDSA - Schnorr signatures over Ed25519.
//!
//!    Musig2 paper: <https://eprint.iacr.org/2020/1261.pdf>
//!
//!    We implement here a two party version.
//!
//!    The number of nonces that each party uses (denoted v in the paper) is set to 2.
//!
//!    We also implement the Musig2* variant (appendix B in the paper) where one of the musig coefficients is set to 1
//!
//!    in order to save some scalar multiplication, this doesn't affect security.

#![allow(non_snake_case)]
#![warn(missing_docs, unsafe_code, future_incompatible)]
mod serde;

mod derive;

use core::fmt;

use curve25519_dalek::constants;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use rand::{thread_rng, Rng};
use sha2::{Digest, Sha512};

/// Errors that may occur while processing signatures and keys
#[derive(Debug)]
pub enum Error {
    /// aggregating 2 pubkeys that are equal is disallowed
    PublicKeysAreEqual,
    /// Public Keys must be valid ed25519 prime order points.
    InvalidPublicKey,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicKeysAreEqual => f.write_str("Public keys Are Equal"),
            Self::InvalidPublicKey => f.write_str("Invalid public key"),
        }
    }
}
impl std::error::Error for Error {}

/// An ed25519 keypair
pub struct KeyPair {
    public_key: EdwardsPoint,
    prefix: [u8; 32],
    private_key: Scalar,
}

impl KeyPair {
    /// Create a new random keypair,
    /// returns the KeyPair, and the Secret Key.
    /// restoring a KeyPair from the secret key can be done using [`KeyPair::create_from_private_key`]
    pub fn create() -> (KeyPair, [u8; 32]) {
        let secret = thread_rng().gen();
        (Self::create_from_private_key(secret), secret)
    }

    /// Create a KeyPair from an existing secret
    pub fn create_from_private_key(secret: [u8; 32]) -> KeyPair {
        // This is according to the ed25519 spec,
        // we use the first half of the hash as the actual private key
        // and the other half as deterministic randomness for the nonce PRF.
        let h = Sha512::new().chain(secret).finalize();
        let mut private_key_bits: [u8; 32] = [0u8; 32];
        let mut prefix: [u8; 32] = [0u8; 32];
        prefix.copy_from_slice(&h[32..64]);
        private_key_bits.copy_from_slice(&h[0..32]);
        private_key_bits[0] &= 248;
        private_key_bits[31] &= 63;
        private_key_bits[31] |= 64;
        let private_key = Scalar::from_bits(private_key_bits);
        let public_key = &private_key * &constants::ED25519_BASEPOINT_TABLE;
        Self {
            public_key,
            prefix,
            private_key,
        }
    }

    /// Exactly like [`Self::partial_sign`] but for a derived key.
    /// Create a partial ed25519 signature,
    /// Combining this with the other party's partial signature will result in a valid ed25519 signature
    pub fn partial_sign_derived(
        &self,
        private_partial_nonce: PrivatePartialNonces,
        public_partial_nonce: [PublicPartialNonces; 2],
        agg_public_key: &AggPublicKeyAndMusigCoeff,
        message: &[u8],
        derived_data: &DerivationData,
    ) -> (PartialSignature, AggregatedNonce) {
        let (mut sig, nonce) = self.partial_sign(
            private_partial_nonce,
            public_partial_nonce,
            agg_public_key,
            message,
        );

        // Only one party needs to adjust the signature, so we limit to just the "first" party in the ordered set.
        if agg_public_key.location == KeySortedLocation::First {
            let challenge = Signature::k(&nonce.0, &agg_public_key.agg_public_key, message);
            sig.0 += derived_data.0 * challenge;
        }
        (sig, nonce)
    }

    /// Create a partial ed25519 signature,
    /// Combining this with the other party's partial signature will result in a valid ed25519 signature
    pub fn partial_sign(
        &self,
        private_partial_nonce: PrivatePartialNonces,
        public_partial_nonce: [PublicPartialNonces; 2],
        agg_public_key: &AggPublicKeyAndMusigCoeff,
        message: &[u8],
    ) -> (PartialSignature, AggregatedNonce) {
        // Sum up the partial nonces from both parties index-wise, meaning,  R[i]
        // is the sum of partial_nonces[i] from both parties
        // NOTE: the number of nonces is v = 2 here!
        let sum_R = [
            public_partial_nonce[0].0[0] + public_partial_nonce[1].0[0],
            public_partial_nonce[0].0[1] + public_partial_nonce[1].0[1],
        ];

        // Compute b as hash of nonces
        // `Scalar::from_hash` reduces the output mod order.
        let b = Scalar::from_hash(
            Sha512::new()
                .chain("musig2 aggregated nonce generation")
                .chain(agg_public_key.agg_public_key.compress().as_bytes())
                .chain(sum_R[0].compress().as_bytes())
                .chain(sum_R[1].compress().as_bytes())
                .chain(message),
        );

        // Compute effective nonce
        // The idea is to compute R and r s.t. R = R_0 + b•R_1 and r = r_0 + b•r_1
        let effective_R = sum_R[0] + b * sum_R[1];
        let effective_r = private_partial_nonce.0[0] + b * private_partial_nonce.0[1];

        // Compute Fiat-Shamir challenge of signature
        let sig_challenge = Signature::k(&effective_R, &agg_public_key.agg_public_key, message);

        let partial_signature =
            effective_r + (agg_public_key.musig_coefficient * self.private_key * sig_challenge);
        (
            PartialSignature(partial_signature),
            AggregatedNonce(effective_R),
        )
    }

    /// Return the public key associated with the KeyPair
    pub fn pubkey(&self) -> [u8; 32] {
        self.public_key.compress().0
    }
}

#[derive(Debug, PartialEq, Eq)]
/// Private Partial Nonces, they should be kept until partially signing a message and then they should be discarded.
///
/// SECURITY: Reusing them across signatures will cause the private key to leak
pub struct PrivatePartialNonces([Scalar; 2]);

impl PrivatePartialNonces {
    /// Serialize the private partial nonces for storage.
    ///
    /// SECURITY: Do not reuse the nonces across signing instances. reusing the nonces will leak the private key.
    pub fn serialize(&self) -> [u8; 64] {
        let mut output = [0u8; 64];
        output[..32].copy_from_slice(&self.0[0].to_bytes());
        output[32..64].copy_from_slice(&self.0[1].to_bytes());
        output
    }

    /// Deserialize the private nonces,
    /// Will return `None` if they're invalid.
    pub fn deserialize(bytes: [u8; 64]) -> Option<Self> {
        Some(Self([
            scalar_from_bytes(&bytes[..32])?,
            scalar_from_bytes(&bytes[32..64])?,
        ]))
    }
}

/// Public partial nonces, they should be transmitted to the other party in order to generate the aggregated nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicPartialNonces([EdwardsPoint; 2]);

impl PublicPartialNonces {
    /// Serialize the public partial nonces in order to transmit the other party.
    pub fn serialize(&self) -> [u8; 64] {
        let mut output = [0u8; 64];
        output[..32].copy_from_slice(&self.0[0].compress().0[..]);
        output[32..64].copy_from_slice(&self.0[1].compress().0[..]);
        output
    }

    /// Deserialize the public partial nonces.
    pub fn deserialize(bytes: [u8; 64]) -> Option<Self> {
        Some(Self([
            edwards_from_bytes(&bytes[..32])?,
            edwards_from_bytes(&bytes[32..64])?,
        ]))
    }
}

/// Generate partial nonces, make sure to call this again for every signing session.
pub fn generate_partial_nonces(
    keys: &KeyPair,
    message: Option<&[u8]>,
) -> (PrivatePartialNonces, PublicPartialNonces) {
    generate_partial_nonces_internal(keys, message, &mut thread_rng())
}

fn generate_partial_nonces_internal(
    keys: &KeyPair,
    message: Option<&[u8]>,
    rng: &mut impl Rng,
) -> (PrivatePartialNonces, PublicPartialNonces) {
    // here we deviate from the spec, by introducing  non-deterministic element (random number)
    // to the nonce, this is important for MPC implementations
    let r: [Scalar; 2] = [(); 2].map(|_| {
        Scalar::from_hash(
            Sha512::new()
                .chain("musig2 private nonce generation")
                .chain(&keys.prefix)
                .chain(message.unwrap_or(&[]))
                .chain(rng.gen::<[u8; 32]>()),
        )
    });
    let R: [EdwardsPoint; 2] = r.map(|scalar| &scalar * &constants::ED25519_BASEPOINT_TABLE);
    (PrivatePartialNonces(r), PublicPartialNonces(R))
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum KeySortedLocation {
    First = 0,
    Second = 1,
}

impl TryFrom<u8> for KeySortedLocation {
    type Error = ();
    fn try_from(a: u8) -> Result<Self, Self::Error> {
        match a {
            a if a == Self::First as u8 => Ok(Self::First),
            a if a == Self::Second as u8 => Ok(Self::Second),
            _ => Err(()),
        }
    }
}

/// This is useful since when aggregating all public keys we also compute our musig coefficient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggPublicKeyAndMusigCoeff {
    agg_public_key: EdwardsPoint,
    musig_coefficient: Scalar,
    location: KeySortedLocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Data required to sign for the derived public key, this is generated when [`AggPublicKeyAndMusigCoeff::derive_key`] is called,
/// and this needs to be passed to [`KeyPair::partial_sign_derived`] when signing
pub struct DerivationData(Scalar);

impl AggPublicKeyAndMusigCoeff {
    /// Aggregate public keys. This creates a combined public key that requires both parties in order to sign messages.
    pub fn aggregate_public_keys(
        my_public_key: [u8; 32],
        other_public_key: [u8; 32],
    ) -> Result<Self, Error> {
        // keys should never be equal since we want the server and client to have different shares of the private key.
        if my_public_key == other_public_key {
            return Err(Error::PublicKeysAreEqual);
        }
        // By section B of the paper, we sort the public keys and set the musig coefficient for the second one as 1.
        let mut keys = [my_public_key, other_public_key];
        keys.sort_unstable();
        let location = if keys[0] == my_public_key {
            KeySortedLocation::First
        } else {
            KeySortedLocation::Second
        };

        let edwards_keys = [
            edwards_from_bytes(&keys[0]).ok_or(Error::InvalidPublicKey)?,
            edwards_from_bytes(&keys[1]).ok_or(Error::InvalidPublicKey)?,
        ];

        let first_musig_coefficient = Scalar::from_hash(
            Sha512::new()
                .chain("musig2 public key aggregation")
                .chain(keys[0])
                .chain(keys[1])
                .chain(keys[0]),
        );

        let agg_public_key = first_musig_coefficient * edwards_keys[0] + edwards_keys[1];

        let musig_coefficient = if location == KeySortedLocation::First {
            first_musig_coefficient
        } else {
            Scalar::one()
        };

        Ok(Self {
            agg_public_key,
            musig_coefficient,
            location,
        })
    }

    /// Derive a child public key
    pub fn derive_key(&self, path: &[u32]) -> (Self, DerivationData) {
        let (delta, agg_public_key) =
            derive::derive_delta_and_public_key_from_path(self.agg_public_key, path);
        (
            Self {
                agg_public_key,
                musig_coefficient: self.musig_coefficient,
                location: self.location,
            },
            DerivationData(delta),
        )
    }

    /// Returns the serialized aggregated public key.
    pub fn aggregated_pubkey(&self) -> [u8; 32] {
        self.agg_public_key.compress().0
    }

    /// Serialize the aggregated public key and the musig coefficient for storage.
    pub fn serialize(&self) -> [u8; 65] {
        let mut output = [0u8; 65];
        output[..32].copy_from_slice(&self.agg_public_key.compress().0[..]);
        output[32..64].copy_from_slice(&self.musig_coefficient.as_bytes()[..]);
        output[64] = self.location as u8;
        output
    }

    /// Deserialize from bytes as [agg_public_key, musig_coefficient].
    pub fn deserialize(bytes: [u8; 65]) -> Option<Self> {
        Some(Self {
            agg_public_key: edwards_from_bytes(&bytes[..32])?,
            musig_coefficient: scalar_from_bytes(&bytes[32..64])?,
            location: bytes[64].try_into().ok()?,
        })
    }
}

/// An Ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    R: EdwardsPoint,
    s: Scalar,
}

/// An invalid signature error
#[derive(Debug, Ord, PartialOrd, PartialEq, Eq)]
pub struct InvalidSignature;

impl fmt::Display for InvalidSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid Signature")
    }
}

impl std::error::Error for InvalidSignature {}

impl Signature {
    /// Aggregate 2 partial signatures together into a single valid ed25519 signature.
    pub fn aggregate_partial_signatures(
        aggregated_nonce: AggregatedNonce,
        partial_sigs: [PartialSignature; 2],
    ) -> Self {
        Self {
            R: aggregated_nonce.0,
            s: partial_sigs[0].0 + partial_sigs[1].0,
        }
    }
    /// Verify an ed25519 signature, this is a strict verification and requires both the public key
    /// and the signature's nonce to only be in the big prime-order sub group.
    pub fn verify(&self, message: &[u8], public_key: [u8; 32]) -> Result<(), InvalidSignature> {
        let A = edwards_from_bytes(&public_key).ok_or(InvalidSignature)?;
        let k = Self::k(&self.R, &A, message);

        let kA = A * k;
        let R_plus_kA = kA + self.R;
        let sG = &self.s * &constants::ED25519_BASEPOINT_TABLE;

        if R_plus_kA == sG {
            Ok(())
        } else {
            Err(InvalidSignature)
        }
    }

    // This is the Fiat-Shamir hash of all protocol state before signing.
    fn k(R: &EdwardsPoint, PK: &EdwardsPoint, message: &[u8]) -> Scalar {
        Scalar::from_hash(
            Sha512::new()
                .chain(R.compress().as_bytes())
                .chain(PK.compress().as_bytes())
                .chain(message),
        )
    }

    /// Serialize the signature
    pub fn serialize(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.R.compress().0[..]);
        out[32..].copy_from_slice(&self.s.as_bytes()[..]);
        out
    }

    /// Deserialize a signature, returns None if the bytes cannot represent a signature.
    pub fn deserialize(bytes: [u8; 64]) -> Option<Self> {
        Some(Self {
            R: edwards_from_bytes(&bytes[..32])?,
            s: scalar_from_bytes(&bytes[32..64])?,
        })
    }
}

/// A partial signature, should be aggregated with another partial signature under the same aggregated public key and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialSignature(Scalar);

impl PartialSignature {
    /// Serialize the partial signature
    pub fn serialize(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Deserialize the partial signature, returns None if the bytes cannot represent a signature.
    pub fn deserialize(bytes: [u8; 32]) -> Option<Self> {
        scalar_from_bytes(&bytes).map(Self)
    }
}

/// The aggregated nonce of both parties, required for aggregating the signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregatedNonce(EdwardsPoint);

impl AggregatedNonce {
    /// Serialize the aggregated nonce
    pub fn serialize(&self) -> [u8; 32] {
        self.0.compress().0
    }

    /// Deserialize the aggregated nonce
    pub fn deserialize(bytes: [u8; 32]) -> Option<Self> {
        edwards_from_bytes(&bytes).map(Self)
    }
}

/// Converts 32 bytes into a Scalar, checking that the scalar is fully reduced.
///
/// # Panics
/// If the input `bytes` slice does not have a length of 32.
#[inline(always)]
fn scalar_from_bytes(bytes: &[u8]) -> Option<Scalar> {
    // Source: https://github.com/dalek-cryptography/ed25519-dalek/blob/ad461f4/src/signature.rs#L85
    let bytes: [u8; 32] = bytes.try_into().unwrap();
    // Since this is only used in signature deserialisation (i.e. upon
    // verification), we can do a "succeed fast" trick by checking that the most
    // significant 4 bits are unset.  If they are unset, we can succeed fast
    // because we are guaranteed that the scalar is fully reduced.  However, if
    // the 4th most significant bit is set, we must do the full reduction check,
    // as the order of the basepoint is roughly a 2^(252.5) bit number.
    //
    // This succeed-fast trick should succeed for roughly half of all scalars.
    if bytes[31] & 240 == 0 {
        Some(Scalar::from_bits(bytes))
    } else {
        Scalar::from_canonical_bytes(bytes)
    }
}

/// Converts 32 bytes into an edwards point.
/// Checks both that the Y coordinate is on the curve, and that the resulting point is torsion free.
///
/// # Panics
/// If the input `bytes` slice does not have a length of 32.
#[inline(always)]
fn edwards_from_bytes(bytes: &[u8]) -> Option<EdwardsPoint> {
    let point = CompressedEdwardsY::from_slice(bytes).decompress()?;
    // We require that the point will be 0 in the small subgroup,
    // `is_small_order()` checks if the point is *only* in the small subgroup,
    // while `is_torsion_free()` makes sure the point is 0 in the small subgroup.
    point.is_torsion_free().then(|| point)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::{
        generate_partial_nonces_internal, AggPublicKeyAndMusigCoeff, DerivationData, KeyPair,
        Signature,
    };
    use curve25519_dalek::scalar::Scalar;
    use ed25519_dalek::Verifier;
    use hex::decode;
    use rand::{thread_rng, Rng};
    use rand_xoshiro::rand_core::{RngCore, SeedableRng};
    use rand_xoshiro::Xoshiro256PlusPlus;

    /// This will generate a fast deterministic rng and will print the seed,
    /// if a test fails, pass in the printed seed to reproduce.
    pub fn deterministic_fast_rand(name: &str, seed: Option<u64>) -> impl Rng {
        let seed = seed.unwrap_or_else(|| thread_rng().gen());
        println!("{} seed: {}", name, seed);
        Xoshiro256PlusPlus::seed_from_u64(seed)
    }

    pub fn verify_dalek(pk: [u8; 32], sig: [u8; 64], msg: &[u8]) -> bool {
        let dalek_pub = ed25519_dalek::PublicKey::from_bytes(&pk).unwrap();
        let dalek_sig = ed25519_dalek::Signature::from_bytes(&sig).unwrap();

        dalek_pub.verify(msg, &dalek_sig).is_ok()
    }

    #[test]
    fn test_generate_pubkey_dalek() {
        let mut rng = deterministic_fast_rand("test_generate_pubkey_dalek", None);

        let mut privkey = [0u8; 32];
        for _ in 0..4096 {
            rng.fill_bytes(&mut privkey);
            let zengo_keypair = KeyPair::create_from_private_key(privkey);
            let dalek_secret = ed25519_dalek::SecretKey::from_bytes(&privkey)
                .expect("Can only fail if bytes.len()<32");
            let dalek_pub = ed25519_dalek::PublicKey::from(&dalek_secret);

            assert_eq!(zengo_keypair.pubkey(), dalek_pub.to_bytes());
        }
    }

    #[test]
    fn test_sign_dalek_verify_zengo() {
        let mut rng = deterministic_fast_rand("test_sign_dalek_verify_zengo", None);

        let mut privkey = [0u8; 32];
        let mut msg = [0u8; 512];
        for msg_len in 0..msg.len() {
            let msg = &mut msg[..msg_len];
            rng.fill_bytes(&mut privkey);
            rng.fill_bytes(msg);
            let dalek_secret = ed25519_dalek::SecretKey::from_bytes(&privkey)
                .expect("Can only fail if bytes.len()<32");
            let dalek_expanded_secret = ed25519_dalek::ExpandedSecretKey::from(&dalek_secret);
            let dalek_pub = ed25519_dalek::PublicKey::from(&dalek_expanded_secret);
            let dalek_sig = dalek_expanded_secret.sign(msg, &dalek_pub);

            let zengo_sig = Signature::deserialize(dalek_sig.to_bytes()).unwrap();
            zengo_sig.verify(msg, dalek_pub.to_bytes()).unwrap();
        }
    }

    #[test]
    fn test_ed25519_generate_keypair_from_seed() {
        let priv_str = "48ab347b2846f96b7bcd00bf985c52b83b92415c5c914bc1f3b09e186cf2b14f"; // Private Key
        let priv_dec: [u8; 32] = decode(priv_str).unwrap().try_into().unwrap();

        let expected_pubkey_hex =
            "c7d17a93f129527bf7ca413f34a0f23c8462a9c3a3edd4f04550a43cdd60b27a";
        let expected_pubkey: [u8; 32] = decode(expected_pubkey_hex).unwrap().try_into().unwrap();

        let keypair = KeyPair::create_from_private_key(priv_dec);
        assert_eq!(
            keypair.pubkey(),
            expected_pubkey,
            "Public keys do not match!"
        );
    }

    #[test]
    fn test_two_party_signing_with_derivation() {
        let mut rng = deterministic_fast_rand("test_two_party_signing_with_derivation", None);

        let mut msg = [0u8; 256];
        let mut derivation = [0u32; 16];
        for msg_len in 0..msg.len() {
            let msg = &mut msg[..msg_len];
            let derivation = &mut derivation[..(msg_len & 0b111)];
            rng.fill(msg);
            rng.fill(derivation);

            let mut simulator = Musig2Simulator::gen_rand(&mut rng);
            simulator.derive_key(derivation);
            let sig = simulator.simulate_sign(msg, &mut rng);

            sig.verify(msg, simulator.agg_pubkey()).unwrap();
            // Verify result against dalek
            assert!(verify_dalek(simulator.agg_pubkey(), sig.serialize(), msg));
        }
    }

    #[test]
    fn test_two_party_signing() {
        let mut rng = deterministic_fast_rand("test_two_party_signing", None);

        let mut msg = [0u8; 256];
        for msg_len in 0..msg.len() {
            let msg = &mut msg[..msg_len];
            rng.fill_bytes(msg);
            let simulator = Musig2Simulator::gen_rand(&mut rng);
            let sig = simulator.simulate_sign(msg, &mut rng);

            sig.verify(msg, simulator.agg_pubkey()).unwrap();
            // Verify result against dalek
            assert!(verify_dalek(simulator.agg_pubkey(), sig.serialize(), msg));
        }
    }

    #[test]
    fn test_invalid_sig() {
        let mut rng = deterministic_fast_rand("test_invalid_sig", None);
        let msg: [u8; 32] = rng.gen();
        let simulator = Musig2Simulator::gen_rand(&mut rng);
        let mut sig = simulator.simulate_sign(&msg, &mut rng);
        sig.s += Scalar::from(1u32);
        sig.verify(&msg, simulator.agg_pubkey()).unwrap_err();
    }

    #[test]
    fn test_equal_pubkeys() {
        let mut rng = deterministic_fast_rand("test_equal_pubkeys", None);
        let keypair = KeyPair::create_from_private_key(rng.gen());
        let pubkey = keypair.pubkey();
        AggPublicKeyAndMusigCoeff::aggregate_public_keys(pubkey, pubkey).unwrap_err();
    }

    struct Musig2Simulator {
        keypair1: KeyPair,
        keypair2: KeyPair,
        aggpubkey1: AggPublicKeyAndMusigCoeff,
        aggpubkey2: AggPublicKeyAndMusigCoeff,
        derivation_data: Option<DerivationData>,
    }

    impl Musig2Simulator {
        fn gen_rand(rng: &mut impl Rng) -> Self {
            let keypair1 = KeyPair::create_from_private_key(rng.gen());
            let keypair2 = KeyPair::create_from_private_key(rng.gen());
            let aggpubkey1 = AggPublicKeyAndMusigCoeff::aggregate_public_keys(
                keypair1.pubkey(),
                keypair2.pubkey(),
            )
            .unwrap();
            let aggpubkey2 = AggPublicKeyAndMusigCoeff::aggregate_public_keys(
                keypair2.pubkey(),
                keypair1.pubkey(),
            )
            .unwrap();

            let sim = Self {
                keypair1,
                keypair2,
                aggpubkey1,
                aggpubkey2,
                derivation_data: None,
            };
            sim.assert_correct_aggkeys();
            sim
        }

        fn assert_correct_aggkeys(&self) {
            assert_eq!(
                self.aggpubkey1.agg_public_key,
                self.aggpubkey2.agg_public_key
            );
            // only one of them should be equal to 1.
            assert_ne!(
                self.aggpubkey1.musig_coefficient == Scalar::one(),
                self.aggpubkey2.musig_coefficient == Scalar::one()
            );
        }

        fn agg_pubkey(&self) -> [u8; 32] {
            self.aggpubkey1.aggregated_pubkey()
        }

        fn derive_key(&mut self, path: &[u32]) {
            let (aggpubkey1, derivation_data1) = self.aggpubkey1.derive_key(path);
            let (aggpubkey2, derivation_data2) = self.aggpubkey2.derive_key(path);
            self.aggpubkey1 = aggpubkey1;
            self.aggpubkey2 = aggpubkey2;
            assert_eq!(derivation_data1, derivation_data2);
            self.derivation_data = Some(derivation_data1);
            self.assert_correct_aggkeys();
        }

        fn simulate_sign(&self, msg: &[u8], rng: &mut impl Rng) -> Signature {
            // randomly either pass `Some(msg)` or `None`.
            let (private_nonces1, public_nonces1) = generate_partial_nonces_internal(
                &self.keypair1,
                rng.gen::<bool>().then(|| msg),
                rng,
            );
            let (private_nonces2, public_nonces2) = generate_partial_nonces_internal(
                &self.keypair2,
                rng.gen::<bool>().then(|| msg),
                rng,
            );

            let sign_function = |keypair, nonce, nonces, agg, msg| match &self.derivation_data {
                Some(derivation_data) => {
                    KeyPair::partial_sign_derived(keypair, nonce, nonces, agg, msg, derivation_data)
                }
                None => KeyPair::partial_sign(keypair, nonce, nonces, agg, msg),
            };

            // Compute partial signatures
            let (partial_sig1, aggregated_nonce1) = sign_function(
                &self.keypair1,
                private_nonces1,
                [public_nonces1.clone(), public_nonces2.clone()],
                &self.aggpubkey1,
                msg,
            );
            let (partial_sig2, aggregated_nonce2) = sign_function(
                &self.keypair2,
                private_nonces2,
                [public_nonces1, public_nonces2],
                &self.aggpubkey2,
                msg,
            );
            assert_eq!(aggregated_nonce1, aggregated_nonce2);
            assert_eq!(aggregated_nonce1.serialize(), aggregated_nonce2.serialize());

            let signature0 = Signature::aggregate_partial_signatures(
                aggregated_nonce1,
                [partial_sig1.clone(), partial_sig2.clone()],
            );
            let signature1 = Signature::aggregate_partial_signatures(
                aggregated_nonce2,
                [partial_sig2, partial_sig1],
            );
            assert_eq!(signature0, signature1);
            assert_eq!(signature0.serialize(), signature1.serialize());
            signature0
        }
    }
}
