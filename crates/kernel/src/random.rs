//! The kernel's random bytes: one seed, taken at boot, expanded forever.
//!
//! A container that asked the host for entropy on every read would be a
//! container whose `/dev/urandom` is a syscall to the outside world — the one
//! thing this design exists to avoid — and one whose every run is
//! unreproducible. So the seed crosses the boundary once, at boot, from
//! `/iso/random/bytes/32`, and everything after it is arithmetic.
//!
//! That has a consequence worth stating plainly: **record and replay work.**
//! Two boots with the same seed produce the same bytes in the same order,
//! which is what makes a failing run reproducible at all. It also means the
//! seed is the whole secret, and a caller that gives every container the same
//! one gives them all the same "random" numbers. Choosing the seed is the
//! host's job, and the mount is where that choice is expressed.
//!
//! ChaCha20 as the expansion, in its RFC 8439 form. Not a hash of a counter
//! and not a linear generator: a container's `/dev/urandom` is where a libc
//! takes its ASLR offsets, its hash seeds and its TLS nonces, and those want
//! a real stream cipher's guarantees rather than something that looked
//! random in a test.

use crate::errno::Errno;

/// The seed's size, and the path it comes from.
pub const SEED_BYTES: usize = 32;

/// A ChaCha20 keystream, read as random bytes.
#[derive(Clone)]
pub struct Random {
    /// `None` until the seed arrives. A container whose host mounted no
    /// `/iso/random` has no entropy, and that is a capability decision made
    /// in configuration — so it is refused by name rather than filled with
    /// zeros, which is the one answer that would be both plausible and
    /// catastrophic.
    state: Option<State>,
}

#[derive(Clone)]
struct State {
    key: [u32; 8],
    /// The block counter. Sixty-four bits of it, so it cannot wrap in any
    /// run: at one 64-byte block per nanosecond it lasts longer than the
    /// universe has.
    counter: u64,
    /// The current block, and how much of it has been handed out.
    block: [u8; 64],
    used: usize,
}

impl Default for Random {
    fn default() -> Self {
        Self::unseeded()
    }
}

impl Random {
    pub const fn unseeded() -> Self {
        Self { state: None }
    }

    pub fn seed(&mut self, seed: &[u8; SEED_BYTES]) {
        let mut key = [0u32; 8];
        for (index, word) in key.iter_mut().enumerate() {
            *word = u32::from_le_bytes(
                seed[index * 4..index * 4 + 4]
                    .try_into()
                    .expect("four bytes"),
            );
        }
        self.state = Some(State {
            key,
            counter: 0,
            block: [0; 64],
            // Nothing generated yet, so the first read makes a block.
            used: 64,
        });
    }

    pub fn is_seeded(&self) -> bool {
        self.state.is_some()
    }

    /// Fills `into` with keystream. Never blocks and never fails once seeded
    /// — which is `getrandom`'s contract for `/dev/urandom` and the reason
    /// the distinction from `/dev/random` does not exist on Linux any more.
    pub fn fill(&mut self, into: &mut [u8]) -> Result<(), Errno> {
        let state = self.state.as_mut().ok_or(Errno::NoDevice)?;
        let mut written = 0;
        while written < into.len() {
            if state.used == 64 {
                // A nonce of zero: a nonce exists so that one key can
                // encrypt several messages, and there is one stream here.
                state.block = block(&state.key, state.counter, [0, 0]);
                state.counter += 1;
                state.used = 0;
            }
            let take = (64 - state.used).min(into.len() - written);
            into[written..written + take]
                .copy_from_slice(&state.block[state.used..state.used + take]);
            state.used += take;
            written += take;
        }
        Ok(())
    }
}

/// One ChaCha20 block, in its RFC 8439 form.
///
/// The counter is what makes each block different, and it is never reused
/// because it only ever counts up. It occupies two words where the RFC puts
/// one counter and the first word of the nonce — which is the standard
/// 64-bit-counter variant, and which leaves two nonce words.
///
/// Those two are a parameter rather than a constant zero, which is the one
/// concession this makes to being testable: the published test vector uses
/// a non-zero nonce, and a `block` that could not be given one could only be
/// checked against itself. The stream passes zeros.
fn block(key: &[u32; 8], counter: u64, nonce: [u32; 2]) -> [u8; 64] {
    // "expand 32-byte k", the constant RFC 8439 fixes.
    let mut state: [u32; 16] = [
        0x6170_7865,
        0x3320_646e,
        0x7962_2d32,
        0x6b20_6574,
        key[0],
        key[1],
        key[2],
        key[3],
        key[4],
        key[5],
        key[6],
        key[7],
        counter as u32,
        (counter >> 32) as u32,
        nonce[0],
        nonce[1],
    ];
    let start = state;
    // Twenty rounds, as ten column-then-diagonal double rounds.
    for _ in 0..10 {
        quarter(&mut state, 0, 4, 8, 12);
        quarter(&mut state, 1, 5, 9, 13);
        quarter(&mut state, 2, 6, 10, 14);
        quarter(&mut state, 3, 7, 11, 15);
        quarter(&mut state, 0, 5, 10, 15);
        quarter(&mut state, 1, 6, 11, 12);
        quarter(&mut state, 2, 7, 8, 13);
        quarter(&mut state, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for index in 0..16 {
        let word = state[index].wrapping_add(start[index]);
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn quarter(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// RFC 8439's own test vector, checked here rather than trusted.
///
/// A stream cipher that is subtly wrong produces bytes that look exactly as
/// random as correct ones — there is no symptom, ever, and no test of
/// "randomness" would find it. The only check that means anything is against
/// the specification's published output.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_function_matches_rfc_8439() {
        // §2.3.2's vector, produced by the *shipping* `block`. An earlier
        // version of this test re-implemented the ten double rounds inline
        // over a hand-built state and never called `block` at all — so
        // corrupting the constant inside `block` left it passing, and every
        // test downstream with it, because those only ever compare one
        // keystream against another.
        //
        // The RFC's state is: the constant, the key, a 32-bit counter of 1,
        // and the nonce `00:00:00:09 00:00:00:4a 00:00:00:00`. This
        // implementation's counter is 64 bits and takes the word the RFC
        // gives to the nonce's first quarter, so the same state is reached
        // with a counter of `1 | 0x09000000 << 32` and the remaining two
        // nonce words passed through.
        let key: [u32; 8] = core::array::from_fn(|index| {
            let base = (index * 4) as u32;
            u32::from_le_bytes([
                base as u8,
                (base + 1) as u8,
                (base + 2) as u8,
                (base + 3) as u8,
            ])
        });
        let produced = block(&key, 1 | (0x0900_0000u64 << 32), [0x4a00_0000, 0]);

        // Compared as the bytes a guest reads, not as words: a mistake in
        // the little-endian serialisation would be invisible otherwise.
        let expected: [u32; 16] = [
            0xe4e7_f110,
            0x1559_3bd1,
            0x1fdd_0f50,
            0xc471_20a3,
            0xc7f4_d1c7,
            0x0368_c033,
            0x9aaa_2204,
            0x4e6c_d4c3,
            0x4664_82d2,
            0x09aa_9f07,
            0x05d7_c214,
            0xa202_8bd9,
            0xd19c_12b5,
            0xb94e_16de,
            0xe883_d0cb,
            0x4e3c_50a2,
        ];
        let mut serialised = [0u8; 64];
        for (index, word) in expected.iter().enumerate() {
            serialised[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        assert_eq!(produced, serialised);
    }

    #[test]
    fn an_unseeded_generator_refuses_rather_than_answering_zeros() {
        let mut random = Random::unseeded();
        let mut bytes = [0u8; 8];
        assert_eq!(random.fill(&mut bytes), Err(Errno::NoDevice));
        assert_eq!(bytes, [0; 8], "and it wrote nothing");
    }

    #[test]
    fn the_same_seed_replays_and_a_different_one_does_not() {
        let mut first = Random::unseeded();
        first.seed(&[7; SEED_BYTES]);
        let mut again = Random::unseeded();
        again.seed(&[7; SEED_BYTES]);
        let mut other = Random::unseeded();
        other.seed(&[8; SEED_BYTES]);

        // Read in uneven pieces, so the block boundary is crossed mid-read:
        // a generator that restarted its block per call would still match on
        // aligned reads and diverge here.
        let mut a = [0u8; 200];
        let mut b = [0u8; 200];
        let mut c = [0u8; 200];
        let mut at = 0;
        for piece in [7usize, 57, 1, 64, 71] {
            let end = (at + piece).min(200);
            first.fill(&mut a[at..end]).expect("seeded");
            at = end;
        }
        assert_eq!(at, 200, "the pieces cover the buffer");
        again.fill(&mut b).expect("seeded");
        other.fill(&mut c).expect("seeded");
        assert_eq!(a, b, "the same seed replays byte for byte");
        assert_ne!(a, c, "a different seed does not");
        // And the stream advances rather than repeating its first block.
        assert_ne!(a[..64], a[64..128]);
    }
}
