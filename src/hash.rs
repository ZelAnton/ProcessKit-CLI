//! Hand-rolled SHA-256 (FIPS 180-4), both incremental and one-shot.
//!
//! processkit-cli hashes two things with the *same* primitive and the same
//! lowercase-hex rendering (`AGENTS.md`, "hand-roll small primitives over a
//! dependency"; one digest style across the project):
//!
//! - the argv fingerprint ([`crate::events`] `argv_sha256`, a one-shot over a
//!   small in-memory buffer), and
//! - bounded stdout/stderr capture ([`crate::capture`], fed **incrementally** as
//!   the child's output streams in, so a multi-megabyte transcript is never held
//!   whole just to hash it).
//!
//! Correctness is pinned against the FIPS 180-4 example vectors in the tests
//! below. This is a stable fingerprint, not an adversarial-integrity primitive:
//! the one property relied on is that the digest does not disclose its input.

/// Initial hash words (FIPS 180-4 §5.3.3): the first 32 bits of the fractional
/// parts of the square roots of the first 8 primes.
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Round constants (FIPS 180-4 §4.2.2): the first 32 bits of the fractional
/// parts of the cube roots of the first 64 primes.
#[rustfmt::skip]
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Incremental SHA-256 state: feed bytes with [`update`](Self::update) as they
/// arrive, then [`finalize`](Self::finalize)/[`finalize_hex`](Self::finalize_hex)
/// once. Cloneable so a caller can snapshot a *running* digest without ending it —
/// the capture path finalizes a clone so its live hasher keeps accumulating.
#[derive(Clone)]
pub struct Sha256 {
    /// The eight working hash words.
    h: [u32; 8],
    /// Bytes buffered toward the next full 64-byte block.
    block: [u8; 64],
    /// How many bytes of `block` are filled (`0..64`).
    block_len: usize,
    /// Total message length in bytes, for the final length padding.
    total_len: u64,
}

impl Sha256 {
    /// A fresh hasher over an empty message.
    pub fn new() -> Self {
        Self {
            h: H0,
            block: [0u8; 64],
            block_len: 0,
            total_len: 0,
        }
    }

    /// Absorb `data` into the running digest. Any number of calls with any chunk
    /// sizes produce the same result as one call with the concatenation.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        // Top off a partial block first, so the full-block fast path below can read
        // straight from the input without a stray copy.
        if self.block_len > 0 {
            let need = 64 - self.block_len;
            let take = need.min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len == 64 {
                let block = self.block;
                compress(&mut self.h, &block);
                self.block_len = 0;
            }
        }
        // Process whole 64-byte blocks straight from the input.
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            compress(&mut self.h, &block);
            data = &data[64..];
        }
        // Stash the remainder for the next call / finalize.
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.block_len = data.len();
        }
    }

    /// Consume the hasher and produce the 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        // Message length in *bits*, needed before padding perturbs the buffer.
        // `block_len` is always in `0..64` here (a full block is compressed as it
        // fills, never stashed), so the terminator index below is always in range.
        let bit_len = self.total_len.wrapping_mul(8);
        // Append the mandatory `0x80` terminator.
        self.block[self.block_len] = 0x80;
        self.block_len += 1;
        // If the 8-byte length field no longer fits (offset 56), zero-fill the rest
        // of this block, compress it, and continue in a fresh one.
        if self.block_len > 56 {
            for byte in &mut self.block[self.block_len..64] {
                *byte = 0;
            }
            let block = self.block;
            compress(&mut self.h, &block);
            self.block_len = 0;
        }
        // Zero-pad up to the length field.
        for byte in &mut self.block[self.block_len..56] {
            *byte = 0;
        }
        // The 64-bit big-endian message length closes the final block.
        self.block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.block;
        compress(&mut self.h, &block);

        let mut out = [0u8; 32];
        for (word, chunk) in self.h.iter().zip(out.chunks_exact_mut(4)) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// The digest as a 64-character lowercase-hex string.
    pub fn finalize_hex(self) -> String {
        to_hex(&self.finalize())
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// SHA-256 of `bytes` as a lowercase-hex string — the one-shot form used by the
/// argv fingerprint.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize_hex()
}

/// The SHA-256 compression function over one 64-byte block (FIPS 180-4 §6.2.2).
fn compress(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (word, src) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for (&ki, &wi) in K.iter().zip(w.iter()) {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(ki)
            .wrapping_add(wi);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *slot = slot.wrapping_add(value);
    }
}

/// Lowercase-hex encoding of `bytes`.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[(byte >> 4) as usize]));
        out.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-rolled SHA-256 against the FIPS 180-4 example vectors — the gate
    /// that lets every digest derived from it (the argv fingerprint, the capture
    /// hashes) be trusted.
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // The 56-byte vector exercises the "length field spills into a second
        // padding block" branch of `finalize`.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// Incremental feeding — in arbitrary chunk sizes and across block boundaries —
    /// yields the identical digest to a single one-shot pass. This is the property
    /// the streaming capture path depends on.
    #[test]
    fn incremental_updates_equal_a_single_pass() {
        // A message longer than several 64-byte blocks, with a non-block-aligned
        // tail, so the chunk splits below cross block boundaries.
        let message: Vec<u8> = (0..200u32).map(|i| (i * 7 + 3) as u8).collect();
        let one_shot = sha256_hex(&message);

        for chunk in [1usize, 3, 31, 60, 64, 65, 127] {
            let mut hasher = Sha256::new();
            for piece in message.chunks(chunk) {
                hasher.update(piece);
            }
            assert_eq!(
                hasher.finalize_hex(),
                one_shot,
                "chunked-by-{chunk} digest must equal the one-shot digest"
            );
        }

        // Byte-at-a-time over a message that is an exact block multiple (128 bytes)
        // also matches — the boundary case where `finalize` needs a whole extra
        // padding block.
        let aligned = vec![0xABu8; 128];
        let mut hasher = Sha256::new();
        for byte in &aligned {
            hasher.update(std::slice::from_ref(byte));
        }
        assert_eq!(hasher.finalize_hex(), sha256_hex(&aligned));
    }

    /// A cloned hasher finalizes independently of the original — the snapshot the
    /// capture path relies on to report a running digest without ending it.
    #[test]
    fn clone_snapshots_without_disturbing_the_original() {
        let mut hasher = Sha256::new();
        hasher.update(b"ab");
        let snapshot = hasher.clone().finalize_hex();
        assert_eq!(snapshot, sha256_hex(b"ab"));
        // The original keeps accumulating past the snapshot.
        hasher.update(b"c");
        assert_eq!(hasher.finalize_hex(), sha256_hex(b"abc"));
    }

    // Property-based tier (T-167). Placed in this same `#[cfg(test)]` module
    // rather than a new `tests/properties.rs`: this crate is bin-only (no
    // `[lib]` target — see K-006), so an integration test under `tests/` cannot
    // link against `sha256_hex`/`Sha256`/`to_hex` at all — only in-module tests
    // run via `cargo test --bin processkit-cli` can reach them.
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// Byte strings with extra density right around the 57–63-byte final-block
        /// spill boundary (K-008 — the known-fragile zone in `finalize`), plus a
        /// broader spread of lengths so the property isn't purely local to that
        /// window.
        fn near_boundary_bytes() -> impl Strategy<Value = Vec<u8>> {
            prop_oneof![
                // Weighted toward the 50..70 band straddling the 57-63 spill zone.
                3 => prop::collection::vec(any::<u8>(), 50..70),
                2 => prop::collection::vec(any::<u8>(), 0..300),
            ]
        }

        /// A byte string plus a sequence of chunk lengths to feed it through
        /// `update` in. Lengths may overshoot what remains (the test caller clamps
        /// each chunk to the data actually left), so any combination is valid input
        /// — no `prop_filter` needed.
        fn bytes_with_splits() -> impl Strategy<Value = (Vec<u8>, Vec<usize>)> {
            (
                prop::collection::vec(any::<u8>(), 0..300),
                prop::collection::vec(0usize..80, 0..8),
            )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            /// The hand-rolled digest agrees with an independent reference
            /// implementation (the `sha2` crate) on every generated input, with
            /// extra density around the historically fragile 57–63-byte boundary.
            #[test]
            fn matches_reference_implementation(data in near_boundary_bytes()) {
                use sha2::Digest as _;
                let mut reference = sha2::Sha256::new();
                reference.update(&data);
                let expected = to_hex(&reference.finalize());
                prop_assert_eq!(sha256_hex(&data), expected);
            }

            /// Splitting the same input across an arbitrary sequence of `update`
            /// calls always yields the same digest as one call over the whole
            /// message — the invariant the streaming capture path
            /// ([`crate::capture`]) depends on.
            #[test]
            fn incremental_split_matches_one_shot((data, splits) in bytes_with_splits()) {
                let one_shot = sha256_hex(&data);

                let mut hasher = Sha256::new();
                let mut start = 0usize;
                for len in splits {
                    let end = (start + len).min(data.len());
                    hasher.update(&data[start..end]);
                    start = end;
                }
                hasher.update(&data[start..]);

                prop_assert_eq!(hasher.finalize_hex(), one_shot);
            }
        }
    }
}
