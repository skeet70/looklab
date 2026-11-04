// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_upload::{
    Challenge, ManifestHash, Nonce, PowError, PowParams, PowSolutions, solve, verify,
};

fn tiny() -> PowParams {
    PowParams {
        memory_kib: 8,
        iterations: 1,
        difficulty_bits_per_image: 8.0,
    }
}

fn challenge() -> Challenge {
    Challenge::new(
        &Nonce::from_bytes([1; 32]),
        &ManifestHash::from_bytes([2; 32]),
    )
}

fn with(solutions: &PowSolutions, index: usize, value: u64) -> PowSolutions {
    let mut array = *solutions.as_array();
    array[index] = value;
    PowSolutions::new(array)
}

#[test]
fn difficulty_tracks_image_count() {
    let params = tiny();
    assert_eq!(params.difficulty_bits(1), Ok(4));
    assert_eq!(params.difficulty_bits(16), Ok(8));
    assert_eq!(params.difficulty_bits(100), Ok(11));
    assert_eq!(params.difficulty_bits(0), Err(PowError::NoImages));
    let impossible = PowParams {
        difficulty_bits_per_image: f64::NAN,
        ..params
    };
    assert_eq!(
        impossible.difficulty_bits(1),
        Err(PowError::NonFiniteDifficulty)
    );
}

#[test]
fn solve_is_deterministic_and_verifies() {
    let params = tiny();
    let challenge = challenge();
    let first = solve(&challenge, 1, &params, 0).expect("tiny parameters solve");
    let second = solve(&challenge, 1, &params, 0).expect("tiny parameters solve");
    assert_eq!(first, second);
    assert_eq!(verify(&challenge, 1, &params, &first), Ok(()));
    let other = solve(&challenge, 1, &params, 7).expect("tiny parameters solve");
    assert_eq!(verify(&challenge, 1, &params, &other), Ok(()));
}

#[test]
fn verify_rejects_a_wrong_sub_puzzle_in_order() {
    let params = tiny();
    let challenge = challenge();
    let solved = solve(&challenge, 1, &params, 0).expect("tiny parameters solve");

    let mut failing = None;
    for value in 0..10_000 {
        let candidate = with(&solved, 5, value);
        if let Err(PowError::InsufficientWork { index, .. }) =
            verify(&challenge, 1, &params, &candidate)
            && index == 5
        {
            failing = Some(value);
            break;
        }
    }
    let failing = failing.expect("some counter fails sub-puzzle 5");
    let candidate = with(&solved, 5, failing);
    assert!(matches!(
        verify(&challenge, 1, &params, &candidate),
        Err(PowError::InsufficientWork {
            index: 5,
            required_bits: 4,
            ..
        })
    ));
}

#[test]
fn verify_rejects_an_unsolved_first_sub_puzzle() {
    let params = tiny();
    let challenge = challenge();
    let solved = solve(&challenge, 1, &params, 0).expect("tiny parameters solve");

    let mut failing = None;
    for value in 0..10_000 {
        let candidate = with(&solved, 0, value);
        if let Err(PowError::InsufficientWork { index: 0, .. }) =
            verify(&challenge, 1, &params, &candidate)
        {
            failing = Some(value);
            break;
        }
    }
    let value = failing.expect("some counter fails sub-puzzle 0");
    let candidate = with(&solved, 0, value);
    assert!(matches!(
        verify(&challenge, 1, &params, &candidate),
        Err(PowError::InsufficientWork { index: 0, .. })
    ));
}

#[test]
fn invalid_argon2_parameters_are_reported() {
    let params = PowParams {
        memory_kib: 0,
        iterations: 1,
        difficulty_bits_per_image: 8.0,
    };
    assert!(matches!(
        solve(&challenge(), 1, &params, 0),
        Err(PowError::InvalidParams(_))
    ));
}
