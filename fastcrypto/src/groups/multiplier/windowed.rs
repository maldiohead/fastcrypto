// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::iter::successors;

use crate::groups::multiplier::integer_utils::{get_bits_from_bytes, test_bit};
use crate::groups::multiplier::{integer_utils, ScalarMultiplier};
use crate::groups::GroupElement;
use crate::serde_helpers::ToFromByteArray;

/// This scalar multiplier uses pre-computation with the windowed method. This multiplier is particularly
/// fast for double multiplications, where a sliding window method is used, but this implies that the
/// `double_mul`, is NOT constant time. However, the single multiplication method `mul` is constant
/// time if the group operations for `G` are constant time.
///
/// The `CACHE_SIZE` should be a power of two. The `SCALAR_SIZE` is the number of bytes in the byte
/// representation of the scalar type `S`, and we assume that the `S::to_byte_array` method returns
/// the scalar in little-endian format.
///
/// The `SLIDING_WINDOW_WIDTH` is the number of bits in the sliding window of the elements not already
/// with precomputed multiples. This should be approximately log2(sqrt(SCALAR_SIZE_IN_BITS)) + 1 for
/// optimal performance.
pub struct WindowedScalarMultiplier<
    G: GroupElement<ScalarType = S>,
    S: GroupElement + ToFromByteArray<SCALAR_SIZE>,
    const CACHE_SIZE: usize,
    const SCALAR_SIZE: usize,
    const SLIDING_WINDOW_WIDTH: usize,
> {
    /// Precomputed multiples of the base element from 0 up to CACHE_SIZE - 1 = 2^WINDOW_WIDTH - 1.
    cache: [G; CACHE_SIZE],
}

impl<
        G: GroupElement<ScalarType = S>,
        S: GroupElement + ToFromByteArray<SCALAR_SIZE>,
        const CACHE_SIZE: usize,
        const SCALAR_SIZE: usize,
        const SLIDING_WINDOW_WIDTH: usize,
    > WindowedScalarMultiplier<G, S, CACHE_SIZE, SCALAR_SIZE, SLIDING_WINDOW_WIDTH>
{
    /// The number of bits in the window. This is equal to the floor of the log2 of the cache size.
    const WINDOW_WIDTH: usize = integer_utils::log2(CACHE_SIZE);
}

impl<
        G: GroupElement<ScalarType = S>,
        S: GroupElement + ToFromByteArray<SCALAR_SIZE>,
        const CACHE_SIZE: usize,
        const SCALAR_SIZE: usize,
        const SLIDING_WINDOW_WIDTH: usize,
    > ScalarMultiplier<G>
    for WindowedScalarMultiplier<G, S, CACHE_SIZE, SCALAR_SIZE, SLIDING_WINDOW_WIDTH>
{
    fn new(base_element: G) -> Self {
        let mut cache = [G::zero(); CACHE_SIZE];
        cache[1] = base_element;
        for i in 2..CACHE_SIZE {
            cache[i] = cache[i - 1] + base_element;
        }
        Self { cache }
    }

    fn mul(&self, scalar: &S) -> G {
        // Scalar as bytes in little-endian representation.
        let scalar_bytes = scalar.to_byte_array();

        let base_2w_expansion = integer_utils::compute_base_2w_expansion::<SCALAR_SIZE>(
            &scalar_bytes,
            Self::WINDOW_WIDTH,
        );

        // Computer multiplication using the fixed-window method to ensure that it's constant time.
        let mut result: G = self.cache[base_2w_expansion[base_2w_expansion.len() - 1]];
        for digit in base_2w_expansion.iter().rev().skip(1) {
            for _ in 1..=Self::WINDOW_WIDTH {
                result = result.double();
            }
            result += self.cache[*digit];
        }
        result
    }

    fn two_scalar_mul(
        &self,
        base_scalar: &G::ScalarType,
        other_element: &G,
        other_scalar: &G::ScalarType,
    ) -> G {
        // Compute the sum of the two multiples using Straus' algorithm combined with a sliding window algorithm.
        multi_scalar_mul(
            &[*base_scalar, *other_scalar],
            &[self.cache[1], *other_element],
            &HashMap::from([(0, self.cache[CACHE_SIZE / 2..CACHE_SIZE].to_vec())]),
            SLIDING_WINDOW_WIDTH,
        )
    }
}

/// This method computes the linear combination of the given scalars and group elements using the
/// sliding window method. Some group elements may have tables of precomputed elements which can
/// be given in the `precomputed` hash map. For the elements which does not have a precomputed table
/// a table of size <i>2<sup>default_window_width</sup> - 1</i> is computed.
///
/// The precomputed tables for an element <i>g</i> should contain the multiples <i>2<sup>w-1</sup> g
/// , ..., (2<sup>w</sup> - 1) g</i> for some integer <i>w > 1</i> which is the window width for the
/// given element.
///
/// The `default_window_width` is the window width for the elements that does not have a precomputation
/// table and may be set to any value >= 1. As rule-of-thumb, this should be set to approximately
/// the bit length of the square root of the scalar size for optimal performance.
pub fn multi_scalar_mul<
    G: GroupElement<ScalarType = S>,
    S: GroupElement + ToFromByteArray<SCALAR_SIZE>,
    const SCALAR_SIZE: usize,
    const N: usize,
>(
    scalars: &[G::ScalarType; N],
    elements: &[G; N],
    precomputed_multiples: &HashMap<usize, Vec<G>>,
    default_window_width: usize,
) -> G {
    let mut window_sizes = [0usize; N];

    // Compute missing precomputation tables.
    let mut missing_precomputations = HashMap::new();
    for (i, element) in elements.iter().enumerate() {
        if !precomputed_multiples.contains_key(&i) {
            missing_precomputations.insert(i, compute_multiples(element, default_window_width));
        }
    }

    // Create vector with all precomputation tables.
    let mut all_precomputed_multiples = vec![];
    for i in 0..N {
        match precomputed_multiples.get(&i).take() {
            Some(precomputed_multiples) => {
                all_precomputed_multiples.push(precomputed_multiples);
                window_sizes[i] = integer_utils::log2(all_precomputed_multiples[i].len()) + 1;
            }
            None => {
                all_precomputed_multiples.push(&missing_precomputations[&i]);
                window_sizes[i] = default_window_width;
            }
        }
    }

    // Compute little-endian byte representations of scalars.
    let scalar_bytes = scalars
        .iter()
        .map(|s| s.to_byte_array())
        .collect::<Vec<[u8; SCALAR_SIZE]>>();

    // We iterate from the top bit and down for all scalars until we reach a set bit. This marks the
    // beginning of a window, and we continue the iteration. When the iterations exists the window,
    // we add the corresponding precomputed value and keeps iterating until the next one bit is found
    // which marks the beginning of the next window.
    let mut is_in_window = [false; N];
    let mut index_in_window = [0usize; N]; // Counter for the current window
    let mut precomputed_multiple_index = [0usize; N];

    // We may skip doubling until result is non-zero.
    let mut is_zero = true;
    let mut result = G::zero();

    // Iterate through all bits of the scalars from the top.
    for bit in (0..SCALAR_SIZE * 8).rev() {
        if !is_zero {
            result = result.double();
        }
        for i in 0..N {
            if is_in_window[i] {
                // A window has been set for this scalar. Keep iterating until the window is finished.
                index_in_window[i] += 1;
                if index_in_window[i] == window_sizes[i] {
                    // This window is finished. Add the right precomputed value and indicate that we are ready for a new window.
                    result = if is_zero {
                        is_zero = false;
                        all_precomputed_multiples[i][precomputed_multiple_index[i]]
                    } else {
                        result + all_precomputed_multiples[i][precomputed_multiple_index[i]]
                    };
                    is_in_window[i] = false;
                }
            } else if test_bit(&scalar_bytes[i], bit) {
                // The iteration has reached a set bit for the i'th scalar.
                if bit >= window_sizes[i] - 1 {
                    // There is enough room for a window. Set indicator and reset window index.
                    is_in_window[i] = true;
                    index_in_window[i] = 1;
                    precomputed_multiple_index[i] = get_bits_from_bytes(
                        &scalar_bytes[i],
                        bit + 1 - window_sizes[i],
                        bit, // The last bit is always one, so we ignore it and only precompute the upper half of the first 2^window_sizes multiples.
                    );
                } else {
                    // There is not enough room left for a window. Continue with regular double-and-add.
                    result = if is_zero {
                        is_zero = false;
                        elements[i]
                    } else {
                        result + elements[i]
                    };
                }
            }
        }
    }
    result
}

/// Compute multiples <i>2<sup>w-1</sup> base_element, (2<sup>w-1</sup> + 1) base_element, ..., (2<sup>w</sup> - 1) base_element</i>.
fn compute_multiples<G: GroupElement>(base_element: &G, window_size: usize) -> Vec<G> {
    assert!(window_size > 0, "Window size must be strictly positive.");
    let mut smallest_multiple = base_element.double();
    for _ in 2..window_size {
        smallest_multiple = smallest_multiple.double();
    }
    successors(Some(smallest_multiple), |g| Some(*g + base_element))
        .take(1 << (window_size - 1))
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use ark_ff::{BigInteger, PrimeField};
    use ark_secp256r1::Fr;
    use rand::thread_rng;

    use crate::groups::ristretto255::{RistrettoPoint, RistrettoScalar};
    use crate::groups::secp256r1::{ProjectivePoint, Scalar};
    use crate::groups::Scalar as ScalarTrait;

    use super::*;

    #[test]
    fn test_scalar_multiplication_ristretto() {
        let multiplier =
            WindowedScalarMultiplier::<RistrettoPoint, RistrettoScalar, 16, 32, 4>::new(
                RistrettoPoint::generator(),
            );

        let scalars = [
            RistrettoScalar::from(0),
            RistrettoScalar::from(1),
            RistrettoScalar::from(2),
            RistrettoScalar::from(1234),
            RistrettoScalar::from(123456),
            RistrettoScalar::from(123456789),
            RistrettoScalar::from(0xffffffffffffffff),
            RistrettoScalar::group_order(),
            RistrettoScalar::group_order() - RistrettoScalar::from(1),
            RistrettoScalar::group_order() + RistrettoScalar::from(1),
        ];

        for scalar in scalars {
            let expected = RistrettoPoint::generator() * scalar;
            let actual = multiplier.mul(&scalar);
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn test_scalar_multiplication_secp256r1() {
        let mut modulus_minus_one = Fr::MODULUS_MINUS_ONE_DIV_TWO;
        modulus_minus_one.mul2();
        let scalars = [
            Scalar::from(0),
            Scalar::from(1),
            Scalar::from(2),
            Scalar::from(1234),
            Scalar::from(123456),
            Scalar::from(123456789),
            Scalar::from(0xffffffffffffffff),
            Scalar(Fr::from(modulus_minus_one)),
        ];

        for scalar in scalars {
            let expected = ProjectivePoint::generator() * scalar;

            let multiplier = WindowedScalarMultiplier::<ProjectivePoint, Scalar, 15, 32, 4>::new(
                ProjectivePoint::generator(),
            );
            let actual = multiplier.mul(&scalar);
            assert_eq!(expected, actual);

            let multiplier = WindowedScalarMultiplier::<ProjectivePoint, Scalar, 16, 32, 4>::new(
                ProjectivePoint::generator(),
            );
            let actual = multiplier.mul(&scalar);
            assert_eq!(expected, actual);

            let multiplier = WindowedScalarMultiplier::<ProjectivePoint, Scalar, 17, 32, 4>::new(
                ProjectivePoint::generator(),
            );
            let actual = multiplier.mul(&scalar);
            assert_eq!(expected, actual);

            let multiplier = WindowedScalarMultiplier::<ProjectivePoint, Scalar, 32, 32, 4>::new(
                ProjectivePoint::generator(),
            );
            let actual = multiplier.mul(&scalar);
            assert_eq!(expected, actual);

            let multiplier = WindowedScalarMultiplier::<ProjectivePoint, Scalar, 64, 32, 4>::new(
                ProjectivePoint::generator(),
            );
            let actual = multiplier.mul(&scalar);
            assert_eq!(expected, actual);

            let multiplier = WindowedScalarMultiplier::<ProjectivePoint, Scalar, 512, 32, 4>::new(
                ProjectivePoint::generator(),
            );
            let actual = multiplier.mul(&scalar);
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn test_double_mul_ristretto() {
        let multiplier =
            WindowedScalarMultiplier::<RistrettoPoint, RistrettoScalar, 16, 32, 5>::new(
                RistrettoPoint::generator(),
            );

        let other_point = RistrettoPoint::generator() * RistrettoScalar::from(3);

        let a = RistrettoScalar::rand(&mut thread_rng());
        let b = RistrettoScalar::rand(&mut thread_rng());
        let expected = RistrettoPoint::generator() * a + other_point * b;
        let actual = multiplier.two_scalar_mul(&a, &other_point, &b);
        assert_eq!(expected, actual);
    }
}
