// SPDX-License-Identifier: AGPL-3.0-or-later
//! Argon2id proof-of-work ([docs/05 §9](../../../docs/05-trust-and-integrity.md)).
//!
//! A batch has [`POW_K`] independent sub-puzzles. Puzzle `j` is solved by the
//! first `x` whose bound hash has at least `d` leading zero bits, where
//!
//! ```text
//! d = round(difficulty_bits_per_image + log2(n_images) - log2(POW_K))
//! ```
//!
//! so the expected number of Argon2 evaluations is
//! `POW_K * 2^d ≈ n_images * 2^difficulty_bits_per_image`. `d` is clamped to
//! `0..=255`.

use argon2::{Algorithm, Argon2, Params, Version};

use crate::nonce::Challenge;

pub const POW_K: usize = 16;

const SALT: &[u8] = b"looklab-pow-v1";

pub const DEFAULT_MEMORY_KIB: u32 = 16 * 1024;
pub const DEFAULT_ITERATIONS: u32 = 1;
pub const DEFAULT_DIFFICULTY_BITS_PER_IMAGE: f64 = 3.0;

/// Arguments to every Argon2id evaluation in a batch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub difficulty_bits_per_image: f64,
}

impl Default for PowParams {
    fn default() -> Self {
        Self {
            memory_kib: DEFAULT_MEMORY_KIB,
            iterations: DEFAULT_ITERATIONS,
            difficulty_bits_per_image: DEFAULT_DIFFICULTY_BITS_PER_IMAGE,
        }
    }
}

/// One solution per sub-puzzle, in puzzle order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowSolutions([u64; POW_K]);

impl PowSolutions {
    pub const fn new(solutions: [u64; POW_K]) -> Self {
        Self(solutions)
    }

    pub const fn as_array(&self) -> &[u64; POW_K] {
        &self.0
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().copied()
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PowError {
    #[error("Argon2 parameters are invalid: {0}")]
    InvalidParams(argon2::Error),
    #[error("difficulty_bits_per_image must be finite")]
    NonFiniteDifficulty,
    #[error("a batch needs at least one image")]
    NoImages,
    #[error("difficulty of {bits} leading zero bits is unreachable")]
    ImpossibleDifficulty { bits: u32 },
    #[error("sub-puzzle {index} ran out of counter values")]
    WorkOverflow { index: usize },
    #[error("sub-puzzle {index} has {found_bits} leading zero bits, needs {required_bits}")]
    InsufficientWork {
        index: usize,
        found_bits: u32,
        required_bits: u32,
    },
}

impl PowParams {
    pub fn difficulty_bits(&self, n_images: u32) -> Result<u32, PowError> {
        if n_images == 0 {
            return Err(PowError::NoImages);
        }
        if !self.difficulty_bits_per_image.is_finite() {
            return Err(PowError::NonFiniteDifficulty);
        }
        let bits = self.difficulty_bits_per_image + libm::log2(f64::from(n_images))
            - libm::log2(POW_K as f64);
        if bits < 0.0 {
            return Ok(0);
        }
        if bits > 255.0 {
            return Err(PowError::ImpossibleDifficulty {
                bits: bits.round() as u32,
            });
        }
        Ok(bits.round() as u32)
    }

    fn argon2(&self) -> Result<Argon2<'static>, PowError> {
        let params = Params::new(self.memory_kib, self.iterations, 1, Some(32))
            .map_err(PowError::InvalidParams)?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

/// Deterministic search from `start` for every sub-puzzle.
pub fn solve(
    challenge: &Challenge,
    n_images: u32,
    params: &PowParams,
    start: u64,
) -> Result<PowSolutions, PowError> {
    let required = params.difficulty_bits(n_images)?;
    let argon = params.argon2()?;
    let mut solutions = [0u64; POW_K];
    for (index, slot) in solutions.iter_mut().enumerate() {
        let mut x = start;
        loop {
            if leading_zero_bits(&evaluate(&argon, challenge, index, x)) >= required {
                *slot = x;
                break;
            }
            x = x.checked_add(1).ok_or(PowError::WorkOverflow { index })?;
        }
    }
    Ok(PowSolutions(solutions))
}

/// Checks the sub-puzzles in order and rejects at the first failure.
pub fn verify(
    challenge: &Challenge,
    n_images: u32,
    params: &PowParams,
    solutions: &PowSolutions,
) -> Result<(), PowError> {
    let required = params.difficulty_bits(n_images)?;
    let argon = params.argon2()?;
    for (index, x) in solutions.iter().enumerate() {
        let found = leading_zero_bits(&evaluate(&argon, challenge, index, x));
        if found < required {
            return Err(PowError::InsufficientWork {
                index,
                found_bits: found,
                required_bits: required,
            });
        }
    }
    Ok(())
}

/// Password is `challenge ‖ j:u8 ‖ x:u64 LE`.
fn evaluate(argon: &Argon2<'_>, challenge: &Challenge, index: usize, x: u64) -> [u8; 32] {
    let mut password = [0u8; 32 + 1 + 8];
    password[..32].copy_from_slice(challenge.as_bytes());
    password[32] = index as u8;
    password[33..].copy_from_slice(&x.to_le_bytes());
    let mut output = [0u8; 32];
    argon
        .hash_password_into(&password, SALT, &mut output)
        .expect("32-byte output is a valid Argon2 request");
    output
}

fn leading_zero_bits(hash: &[u8; 32]) -> u32 {
    let mut bits = 0;
    for byte in hash {
        bits += byte.leading_zeros();
        if *byte != 0 {
            break;
        }
    }
    bits
}
