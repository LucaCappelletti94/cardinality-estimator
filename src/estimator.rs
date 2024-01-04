//! Cardinality estimator allows to estimate number of distinct elements
//! in the stream or dataset and is defined with const `P` and `W` parameters:
//! - `P`: precision parameter in [4..18] range, which defines
//!   number of bits to use for HyperLogLog register indices.
//! - `W`: width parameter in [4..6] range, which defines
//!   number of bits to use for HyperLogLog register width.
//!
//! # Data-structure design rationale
//!
//! ## Low memory footprint
//!
//! For parameters P = 12, W = 6:
//! - Cardinality in [0..2] range - 8 bytes (small representation)
//! - Cardinality in [3..4] range - 24 bytes (slice representation)
//! - Cardinality in [5..8] range - 40 bytes (slice representation)
//! - Cardinality in [9..16] range - 72 bytes (slice representation)
//! - Cardinality in [17..28] range - 184 bytes (hashset representation)
//! - ...
//! - Cardinality in [449..] range - 3092 bytes (hyperloglog representation)
//!
//! ## Low latency
//! - Auto-vectorization for slice operations via compiler hints
//!   to use SIMD instructions when using `chunks_exact`.
//! - Number of zero registers and registers' harmonic sum are
//!   stored and updated dynamically as more data being inserted,
//!   allowing to have truly constant `estimate` operations.
//! - Efficient polynomial computation using Horner's method.
//!
//! ## High accuracy
//! - For small cardinality range (<= 448 for P = 12, W = 6)
//!   cardinality counted very accurately (within hash collisions chance)
//! - For large cardinality range HyperLogLog++ is used with LogLog-Beta bias correction.
//!   - Expected error:
//!     P = 10, W = 5: 1.04 / sqrt(2^10) = 3.25%
//!     P = 12, W = 6: 1.04 / sqrt(2^12) = 1.62%
//!     P = 14, W = 6: 1.04 / sqrt(2^14) = 0.81%
//!     P = 18, W = 6: 1.04 / sqrt(2^18) = 0.02%
//!
//! # Data storage format
//! Cardinality estimator stores data in one of the four representations:
//!
//! ## Small representation
//! Allows to estimate cardinality in [0..2] range and uses only 8 bytes of memory.
//!
//! The `data` format of small representation:
//! - 0..1 bits     - store representation type (bits are set to `00`)
//! - 2..33 bits    - store 31-bit encoded hash
//! - 34..63 bits   - store 31-bit encoded hash
//!
//! ## Slice representation
//! Allows to estimate small cardinality in [3..16] range.
//!
//! The `data` format of slice representation:
//! - 0..1 bits     - store representation type (bits are set to `01`)
//! - 2..55 bits    - store pointer to `u32` slice (on `x86_64 systems only 48-bits are needed).
//! - 56..63 bits   - store actual slice length
//!
//! Slice encoding:
//! - data[0..N]    - store N `u32` encoded hashes
//! - data[N..]     - store zeros used for future hashes
//!
//! ## HashSet representation
//! Allows to estimate small cardinality in [16..N] range, where `N` based on `P` and `W` parameters.
//!
//! The `data` format of hashset representation:
//! - 0..1 bits     - store representation type (bits are set to `10`)
//! - 2..63 bits    - store pointer to `Box<HashSet<u32>>` (on `x86_64 systems only 48-bits are needed).
//!
//! ## HyperLogLog representation
//! Allows to estimate large cardinality in `[N..]` range, where `N` is based on `P` and `W`.
//! This representation uses modified HyperLogLog++ with `M` registers of `W` width.
//!
//! Original HyperLogLog++ paper:
//! https://static.googleusercontent.com/media/research.google.com/en//pubs/archive/40671.pdf
//!
//! The `data` format of HyperLogLog representation:
//! - 0..1 bits     - store representation type (bits are set to `11`)
//! - 2..63 bits    - store pointer to `u32` slice (on `x86_64 systems only 48-bits are needed).
//!
//! Slice encoding:
//! - data[0]       - stores number of HyperLogLog registers set to 0.
//! - data[1]       - stores harmonic sum of HyperLogLog registers (`f32` transmuted into `u32`).
//! - data[2..]     - stores register ranks using `W` bits per each register.

use std::fmt::{Debug, Formatter};
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher};
use std::mem::{size_of, size_of_val};
use std::slice;

use crate::beta::beta_horner;
use Representation::*;

use hashbrown::HashSet;
use wyhash::WyHash;

/// Maximum number of elements stored in slice representation
const MAX_SLICE_CAPACITY: usize = 16;
/// Mask used for storing and retrieving representation type stored in lowest 2 bits of `data` field.
const REPRESENTATION_MASK: usize = 0x0000_0000_0000_0003;
/// Mask used for accessing heap allocated data stored at the pointer in `data` field.
const PTR_MASK: usize = 0x00ff_ffff_ffff_fffc;
/// Mask used for extracting hashes stored in small representation (31 bits)
const SMALL_MASK: usize = 0x0000_0000_7fff_ffff;

/// Ensure that only 64-bit architecture is being used.
#[cfg(target_pointer_width = "64")]
pub struct CardinalityEstimator<
    const P: usize = 12,
    const W: usize = 6,
    H: Hasher + Default = WyHash,
> {
    /// Raw data format described above
    pub(crate) data: usize,
    /// Zero-sized build hasher
    build_hasher: BuildHasherDefault<H>,
}

/// Four representation types supported by `CardinalityEstimator`
#[repr(u8)]
#[derive(Debug, PartialEq)]
pub enum Representation {
    Small = 0,
    Slice = 1,
    HashSet = 2,
    HyperLogLog = 3,
}

impl<const P: usize, const W: usize, H: Hasher + Default> CardinalityEstimator<P, W, H> {
    /// Ensure that `P` and `W` are in correct range at compile time
    const VALID_PARAMS: () = assert!(P >= 4 && P <= 18 && W >= 4 && W <= 6);
    /// Number of HyperLogLog registers
    const M: usize = 1 << P;
    /// HyperLogLog representation `u32` slice length based on #registers, stored zero registers, harmonic sum, and
    /// one extra element for branchless register updates (see `set_register` for more details).
    const HLL_SLICE_LEN: usize = Self::M * W / 32 + 3;

    /// Creates new instance of `CardinalityEstimator`
    #[inline]
    pub fn new() -> Self {
        // compile time check of params
        _ = Self::VALID_PARAMS;

        Self {
            // Start with empty small representation
            data: 0,
            build_hasher: BuildHasherDefault::default(),
        }
    }

    /// Return representation type of `CardinalityEstimator`
    #[inline]
    pub fn representation(&self) -> Representation {
        // SAFETY: representation is always one of four types stored in lowest 2 bits of `data` field.
        unsafe { std::mem::transmute((self.data & REPRESENTATION_MASK) as u8) }
    }

    /// Insert a hashable item into `CardinalityEstimator`
    #[inline]
    pub fn insert<T: Hash + ?Sized>(&mut self, item: &T) {
        let mut hasher = self.build_hasher.build_hasher();
        item.hash(&mut hasher);
        let hash = hasher.finish();
        self.insert_hash(hash);
    }

    /// Insert hash into `CardinalityEstimator`
    #[inline]
    pub fn insert_hash(&mut self, hash: u64) {
        if self.representation() == HyperLogLog {
            let idx = (hash & ((1 << P) - 1)) as u32;
            let rank = (!hash >> P).trailing_zeros() + 1;
            Self::insert_into_hll(self.as_hll_slice_mut(), idx, rank);
        } else {
            self.insert_encoded_hash(Self::encode_hash(hash));
        }
    }

    /// Insert encoded hash into `CardinalityEstimator`
    #[inline]
    fn insert_encoded_hash(&mut self, h: u32) {
        match self.representation() {
            Small => self.insert_into_small(h),
            Slice => self.insert_into_slice(h),
            HashSet => self.insert_into_set(h),
            HyperLogLog => {
                let (idx, rank) = Self::decode_hash(h);
                Self::insert_into_hll(self.as_hll_slice_mut(), idx, rank);
            }
        }
    }

    /// Return cardinality estimate
    #[inline]
    pub fn estimate(&self) -> usize {
        match self.representation() {
            Small => self.estimate_small(),
            Slice => self.slice_len(),
            HashSet => self.as_hashset().len(),
            HyperLogLog => self.estimate_hll(),
        }
    }

    /// Merge cardinality estimators
    #[inline]
    pub fn merge(&mut self, rhs: &Self) {
        match (self.representation(), rhs.representation()) {
            (_, Small) => {
                let h1 = rhs.small_h1();
                let h2 = rhs.small_h2();
                if h1 != 0 {
                    self.insert_encoded_hash(h1);
                }
                if h2 != 0 {
                    self.insert_encoded_hash(h2);
                }
            }
            (_, Slice) => {
                for &h in &rhs.as_slice()[..rhs.slice_len()] {
                    self.insert_encoded_hash(h);
                }
            }
            (_, HashSet) => {
                for &h in rhs.as_hashset() {
                    self.insert_encoded_hash(h);
                }
            }
            (Small, HyperLogLog) => {
                let mut data = rhs.as_hll_slice().to_vec();
                let h1 = self.small_h1();
                let h2 = self.small_h2();
                if h1 != 0 {
                    let (idx, rank) = Self::decode_hash(h1);
                    Self::insert_into_hll(&mut data, idx, rank);
                }
                if h2 != 0 {
                    let (idx, rank) = Self::decode_hash(h2);
                    Self::insert_into_hll(&mut data, idx, rank);
                }
                self.set_hll_data(data);
            }
            (Slice, HyperLogLog) => {
                let slice_len = self.slice_len();
                let slice_data = self.as_slice_mut();
                let mut data = rhs.as_hll_slice().to_vec();
                for &h in &slice_data[..slice_len] {
                    let (idx, rank) = Self::decode_hash(h);
                    Self::insert_into_hll(&mut data, idx, rank);
                }
                drop(unsafe { Box::from_raw(slice_data) });
                self.set_hll_data(data);
            }
            (HashSet, HyperLogLog) => {
                let hashset_data = self.as_hashset_mut();
                let mut data = rhs.as_hll_slice().to_vec();
                for &h in hashset_data.iter() {
                    let (idx, rank) = Self::decode_hash(h);
                    Self::insert_into_hll(&mut data, idx, rank);
                }
                drop(unsafe { Box::from_raw(hashset_data) });
                self.set_hll_data(data);
            }
            (HyperLogLog, HyperLogLog) => {
                let lhs_data = self.as_hll_slice_mut();
                let rhs_data = rhs.as_hll_slice();
                for idx in 0..Self::M as u32 {
                    let lhs_rank = get_register::<W>(lhs_data, idx);
                    let rhs_rank = get_register::<W>(rhs_data, idx);
                    if rhs_rank > lhs_rank {
                        set_register::<W>(lhs_data, idx, lhs_rank, rhs_rank);
                    }
                }
            }
        }
    }

    /// Return 1-st encoded hash assuming small representation
    #[inline]
    fn small_h1(&self) -> u32 {
        ((self.data >> 2) & SMALL_MASK) as u32
    }

    /// Return 2-nd encoded hash assuming small representation
    #[inline]
    fn small_h2(&self) -> u32 {
        ((self.data >> 33) & SMALL_MASK) as u32
    }

    /// Insert encoded hash into small representation
    /// with potential upgrade to slice representation
    #[inline]
    fn insert_into_small(&mut self, h: u32) {
        // Retrieve 1-st encoded hash
        let h1 = self.small_h1();
        if h1 == 0 {
            self.data |= (h as usize) << 2;
            return;
        }
        if h1 == h {
            return;
        }
        // Retrieve 2-nd encoded hash
        let h2 = self.small_h2();
        if h2 == 0 {
            self.data |= (h as usize) << 33;
            return;
        }
        if h2 == h {
            return;
        }

        // both hashes occupied -> upgrade to slice representation
        self.set_slice_data(vec![h1, h2, h, 0], 3);
    }

    /// Insert encoded hash into slice representation
    #[inline]
    fn insert_into_slice(&mut self, h: u32) {
        let len = self.slice_len();
        let data = self.as_slice_mut();
        let cap = data.len();

        let found = if cap == 4 {
            contains_vectorized::<4>(&data, h)
        } else {
            // calculate rounded up slice length for efficient look up in batches
            let rlen = 8 * len.div_ceil(8);
            contains_vectorized::<8>(&data[..rlen], h)
        };

        if found {
            return;
        }

        if len < cap {
            // if there are available slots in current slice - append to it
            *unsafe { data.get_unchecked_mut(len) } = h;
            self.data = ((len + 1) << 56) | (PTR_MASK & data.as_ptr() as usize) | (Slice as usize);
            return;
        }

        if cap < MAX_SLICE_CAPACITY {
            let mut new_data = vec![0; cap * 2];
            new_data[..len].copy_from_slice(data);
            new_data[len] = h;
            drop(unsafe { Box::from_raw(data) });
            self.set_slice_data(new_data, len + 1);
        } else {
            let mut set = Box::new(HashSet::with_capacity(cap + 1));
            for h in data {
                set.insert(*h);
            }
            set.insert(h);

            let ptr = Box::into_raw(set);
            self.data = (ptr as usize) | (HashSet as usize);
        }
    }

    /// Insert encoded hash into hashset representation
    #[inline]
    fn insert_into_set(&mut self, h: u32) {
        let set = self.as_hashset_mut();

        if set.capacity() == set.len() {
            let (_, layout) = set.raw_table().allocation_info();
            // if doubling hashset capacity exceeds HyperLogLog representation size - migrate to it
            if 2 * layout.size() > Self::HLL_SLICE_LEN * size_of::<u32>() {
                let mut data = vec![0; Self::HLL_SLICE_LEN];
                data[0] = Self::M as u32;
                data[1] = (Self::M as f32).to_bits();

                for &h in set.iter() {
                    let (idx, new_rank) = Self::decode_hash(h);
                    Self::insert_into_hll(&mut data, idx, new_rank);
                }

                drop(unsafe { Box::from_raw(set) });
                self.set_hll_data(data);
                self.insert_encoded_hash(h);

                return;
            }
        }

        set.insert(h);
    }

    /// Set slice representation with new data
    #[inline]
    pub(crate) fn set_slice_data(&mut self, data: Vec<u32>, len: usize) {
        self.data = (len << 56) | (PTR_MASK & data.as_ptr() as usize) | (Slice as usize);
        std::mem::forget(data);
    }

    /// Set HyperLogLog representation with new data
    #[inline]
    fn set_hll_data(&mut self, data: Vec<u32>) {
        self.data = (PTR_MASK & (data.as_ptr() as usize)) | (HyperLogLog as usize);
        std::mem::forget(data);
    }

    /// Compute the sparse encoding of the given hash
    #[inline]
    fn encode_hash(hash: u64) -> u32 {
        let idx = (hash as u32) & ((1 << (32 - W - 1)) - 1);
        let rank = (!hash >> P).trailing_zeros() + 1;
        (idx << W) | rank
    }

    /// Return normal index and rank from encoded sparse hash
    #[inline]
    fn decode_hash(h: u32) -> (u32, u32) {
        let rank = h & ((1 << W) - 1);
        let idx = (h >> W) & ((1 << P) - 1);
        (idx, rank)
    }

    /// Return cardinality estimate of small representation
    #[inline]
    fn estimate_small(&self) -> usize {
        match (self.small_h1(), self.small_h2()) {
            (0, 0) => 0,
            (_, 0) => 1,
            (_, _) => 2,
        }
    }

    /// Return cardinality estimate of slice representation
    #[inline]
    fn slice_len(&self) -> usize {
        self.data >> 56
    }

    /// Return underlying slice of `u32` for slice representation
    #[inline]
    fn as_slice(&self) -> &[u32] {
        let ptr = (self.data & PTR_MASK) as *const u32;
        let cap = self.slice_len().next_power_of_two();
        unsafe { slice::from_raw_parts(ptr, cap) }
    }

    /// Return mutable underlying slice of `u32` for slice representation
    #[inline]
    fn as_slice_mut(&mut self) -> &mut [u32] {
        let ptr = (self.data & PTR_MASK) as *mut u32;
        let cap = self.slice_len().next_power_of_two();
        unsafe { slice::from_raw_parts_mut(ptr, cap) }
    }

    /// Return underlying `HashSet` of `u32` for hashset representation
    #[inline]
    fn as_hashset(&self) -> &HashSet<u32> {
        unsafe { &*((self.data & PTR_MASK) as *const HashSet<u32>) }
    }

    /// Return mutable underlying `HashSet` of `u32` for hashset representation
    #[inline]
    fn as_hashset_mut(&mut self) -> &mut HashSet<u32> {
        unsafe { &mut *((self.data & PTR_MASK) as *mut HashSet<u32>) }
    }

    /// Return underlying slice of `u32` for HyperLogLog representation
    #[inline]
    fn as_hll_slice(&self) -> &[u32] {
        let ptr = (self.data & PTR_MASK) as *const u32;
        unsafe { slice::from_raw_parts(ptr, Self::HLL_SLICE_LEN) }
    }

    /// Return mutable underlying slice of `u32` for HyperLogLog representation
    #[inline]
    fn as_hll_slice_mut(&mut self) -> &mut [u32] {
        let ptr = (self.data & PTR_MASK) as *mut u32;
        unsafe { slice::from_raw_parts_mut(ptr, Self::HLL_SLICE_LEN) }
    }

    /// Insert encoded hash into HyperLogLog representation
    #[inline]
    fn insert_into_hll(data: &mut [u32], idx: u32, new_rank: u32) {
        let old_rank = get_register::<W>(data, idx);
        if new_rank > old_rank {
            set_register::<W>(data, idx, old_rank, new_rank);
        }
    }

    /// Return cardinality estimate of HyperLogLog representation
    #[inline]
    fn estimate_hll(&self) -> usize {
        let data = self.as_hll_slice();
        let zeros = unsafe { *data.get_unchecked(0) };
        let sum = f32::from_bits(unsafe { *data.get_unchecked(1) }) as f64;
        let estimate = alpha(Self::M) * ((Self::M * (Self::M - zeros as usize)) as f64)
            / (sum + beta_horner(zeros as f64, P));
        (estimate + 0.5) as usize
    }

    /// Return memory size of `CardinalityEstimator`
    pub fn size_of(&self) -> usize {
        size_of::<Self>()
            + match self.representation() {
                Small => 0,
                Slice => size_of_val(self.as_slice()),
                HashSet => {
                    let (_, layout) = self.as_hashset().raw_table().allocation_info();
                    layout.size()
                }
                HyperLogLog => size_of_val(self.as_hll_slice()),
            }
    }
}

impl<const P: usize, const W: usize, H: Hasher + Default> Default
    for CardinalityEstimator<P, W, H>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<const P: usize, const W: usize, H: Hasher + Default> Clone for CardinalityEstimator<P, W, H> {
    /// Clone `CardinalityEstimator`
    fn clone(&self) -> Self {
        let mut estimator = Self::new();
        estimator.merge(self);
        estimator
    }
}

impl<const P: usize, const W: usize, H: Hasher + Default> Drop for CardinalityEstimator<P, W, H> {
    /// Free memory occupied by `CardinalityEstimator`
    fn drop(&mut self) {
        match self.representation() {
            Small => {}
            Slice => {
                drop(unsafe { Box::from_raw(self.as_slice_mut()) });
            }
            HashSet => {
                drop(unsafe { Box::from_raw(self.as_hashset_mut()) });
            }
            HyperLogLog => {
                drop(unsafe { Box::from_raw(self.as_hll_slice_mut()) });
            }
        }
    }
}

impl<const P: usize, const W: usize, H: Hasher + Default> PartialEq
    for CardinalityEstimator<P, W, H>
{
    /// Compare cardinality estimators
    fn eq(&self, rhs: &Self) -> bool {
        if self.representation() != rhs.representation() {
            return false;
        }

        match self.representation() {
            Small => self.data == rhs.data,
            Slice => self.as_slice() == rhs.as_slice(),
            HashSet => self.as_hashset() == rhs.as_hashset(),
            HyperLogLog => self.as_hll_slice() == rhs.as_hll_slice(),
        }
    }
}

impl<const P: usize, const W: usize, H: Hasher + Default> Debug for CardinalityEstimator<P, W, H> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ representation: {:?}, estimate: {}, size: {} }}",
            self.representation(),
            self.estimate(),
            self.size_of()
        )
    }
}

/// Vectorized linear array search benefiting from SIMD instructions (e.g. AVX2).
///
/// Input slice length assumed to be divisible by `N` to perform efficient
/// batch comparisons of slice elements to provided value `v`.
///
/// Assembly output: https://godbolt.org/z/eb8Kob9fa
/// Background reading: https://tinyurl.com/2e4srh2d
fn contains_vectorized<const N: usize>(a: &[u32], v: u32) -> bool {
    a.chunks_exact(N)
        .any(|chunk| contains_fixed_vectorized::<N>(chunk.try_into().unwrap(), v))
}

/// Vectorized linear fixed array search
#[inline]
fn contains_fixed_vectorized<const N: usize>(a: [u32; N], v: u32) -> bool {
    let mut res = false;
    for x in a {
        res |= x == v
    }
    res
}

/// Parameter for bias correction
#[inline]
fn alpha(m: usize) -> f64 {
    match m {
        16 => 0.673,
        32 => 0.697,
        64 => 0.709,
        _ => 0.7213 / (1.0 + 1.079 / (m as f64)),
    }
}

/// Get HyperLogLog `idx` register
#[inline]
fn get_register<const W: usize>(data: &[u32], idx: u32) -> u32 {
    let bit_idx = (idx as usize) * W;
    let u32_idx = (bit_idx / 32) + 2;
    let bit_pos = bit_idx % 32;
    let bits = unsafe { data.get_unchecked(u32_idx..u32_idx + 2) };
    let bits_1 = W.min(32 - bit_pos);
    let bits_2 = W - bits_1;
    let mask_1 = (1 << bits_1) - 1;
    let mask_2 = (1 << bits_2) - 1;

    ((bits[0] >> bit_pos) & mask_1) | ((bits[1] & mask_2) << bits_1)
}

/// Set HyperLogLog `idx` register to new value `rank`
#[inline]
fn set_register<const W: usize>(data: &mut [u32], idx: u32, old_rank: u32, new_rank: u32) {
    let bit_idx = (idx as usize) * W;
    let u32_idx = (bit_idx / 32) + 2;
    let bit_pos = bit_idx % 32;

    let bits = unsafe { data.get_unchecked_mut(u32_idx..u32_idx + 2) };
    let bits_1 = W.min(32 - bit_pos);
    let bits_2 = W - bits_1;
    let mask_1 = (1 << bits_1) - 1;
    let mask_2 = (1 << bits_2) - 1;

    // Unconditionally update two `u32` elements based on `new_rank` bits and masks
    bits[0] &= !(mask_1 << bit_pos);
    bits[0] |= (new_rank & mask_1) << bit_pos;
    bits[1] &= !mask_2;
    bits[1] |= (new_rank >> bits_1) & mask_2;

    // Update HyperLogLog's number of zero registers and harmonic sum
    let zeros_and_sum = unsafe { data.get_unchecked_mut(0..2) };
    zeros_and_sum[0] -= (old_rank == 0) as u32 & (zeros_and_sum[0] > 0) as u32;

    let mut sum = f32::from_bits(zeros_and_sum[1]);
    sum -= 1.0 / ((1u64 << (old_rank as u64)) as f32);
    sum += 1.0 / ((1u64 << (new_rank as u64)) as f32);
    zeros_and_sum[1] = sum.to_bits();
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(0 => "estimator = { representation: Small, estimate: 0, size: 8 } avg_err = 0.0000")]
    #[test_case(1 => "estimator = { representation: Small, estimate: 1, size: 8 } avg_err = 0.0000")]
    #[test_case(2 => "estimator = { representation: Small, estimate: 2, size: 8 } avg_err = 0.0000")]
    #[test_case(3 => "estimator = { representation: Slice, estimate: 3, size: 24 } avg_err = 0.0000")]
    #[test_case(4 => "estimator = { representation: Slice, estimate: 4, size: 24 } avg_err = 0.0000")]
    #[test_case(8 => "estimator = { representation: Slice, estimate: 8, size: 40 } avg_err = 0.0000")]
    #[test_case(16 => "estimator = { representation: Slice, estimate: 16, size: 72 } avg_err = 0.0000")]
    #[test_case(17 => "estimator = { representation: HashSet, estimate: 17, size: 184 } avg_err = 0.0000")]
    #[test_case(28 => "estimator = { representation: HashSet, estimate: 28, size: 184 } avg_err = 0.0000")]
    #[test_case(29 => "estimator = { representation: HashSet, estimate: 29, size: 344 } avg_err = 0.0000")]
    #[test_case(56 => "estimator = { representation: HashSet, estimate: 56, size: 344 } avg_err = 0.0000")]
    #[test_case(57 => "estimator = { representation: HyperLogLog, estimate: 59, size: 660 } avg_err = 0.0006")]
    #[test_case(128 => "estimator = { representation: HyperLogLog, estimate: 130, size: 660 } avg_err = 0.0092")]
    #[test_case(256 => "estimator = { representation: HyperLogLog, estimate: 264, size: 660 } avg_err = 0.0165")]
    #[test_case(512 => "estimator = { representation: HyperLogLog, estimate: 512, size: 660 } avg_err = 0.0174")]
    #[test_case(1024 => "estimator = { representation: HyperLogLog, estimate: 1033, size: 660 } avg_err = 0.0184")]
    #[test_case(10_000 => "estimator = { representation: HyperLogLog, estimate: 10417, size: 660 } avg_err = 0.0282")]
    #[test_case(100_000 => "estimator = { representation: HyperLogLog, estimate: 93099, size: 660 } avg_err = 0.0351")]
    fn test_estimator_p10_w5(n: usize) -> String {
        evaluate_cardinality_estimator(CardinalityEstimator::<10, 5>::new(), n)
    }

    #[test_case(0 => "estimator = { representation: Small, estimate: 0, size: 8 } avg_err = 0.0000")]
    #[test_case(1 => "estimator = { representation: Small, estimate: 1, size: 8 } avg_err = 0.0000")]
    #[test_case(2 => "estimator = { representation: Small, estimate: 2, size: 8 } avg_err = 0.0000")]
    #[test_case(3 => "estimator = { representation: Slice, estimate: 3, size: 24 } avg_err = 0.0000")]
    #[test_case(4 => "estimator = { representation: Slice, estimate: 4, size: 24 } avg_err = 0.0000")]
    #[test_case(8 => "estimator = { representation: Slice, estimate: 8, size: 40 } avg_err = 0.0000")]
    #[test_case(16 => "estimator = { representation: Slice, estimate: 16, size: 72 } avg_err = 0.0000")]
    #[test_case(17 => "estimator = { representation: HashSet, estimate: 17, size: 184 } avg_err = 0.0000")]
    #[test_case(28 => "estimator = { representation: HashSet, estimate: 28, size: 184 } avg_err = 0.0000")]
    #[test_case(29 => "estimator = { representation: HashSet, estimate: 29, size: 344 } avg_err = 0.0000")]
    #[test_case(56 => "estimator = { representation: HashSet, estimate: 56, size: 344 } avg_err = 0.0000")]
    #[test_case(256 => "estimator = { representation: HashSet, estimate: 256, size: 2584 } avg_err = 0.0000")]
    #[test_case(448 => "estimator = { representation: HashSet, estimate: 448, size: 2584 } avg_err = 0.0000")]
    #[test_case(449 => "estimator = { representation: HyperLogLog, estimate: 442, size: 3092 } avg_err = 0.0000")]
    #[test_case(512 => "estimator = { representation: HyperLogLog, estimate: 498, size: 3092 } avg_err = 0.0027")]
    #[test_case(1024 => "estimator = { representation: HyperLogLog, estimate: 1012, size: 3092 } avg_err = 0.0110")]
    #[test_case(4096 => "estimator = { representation: HyperLogLog, estimate: 4105, size: 3092 } avg_err = 0.0084")]
    #[test_case(10_000 => "estimator = { representation: HyperLogLog, estimate: 10068, size: 3092 } avg_err = 0.0085")]
    #[test_case(100_000 => "estimator = { representation: HyperLogLog, estimate: 95628, size: 3092 } avg_err = 0.0182")]
    fn test_estimator_p12_w6(n: usize) -> String {
        evaluate_cardinality_estimator(CardinalityEstimator::<12, 6>::new(), n)
    }

    #[test_case(0 => "estimator = { representation: Small, estimate: 0, size: 8 } avg_err = 0.0000")]
    #[test_case(1 => "estimator = { representation: Small, estimate: 1, size: 8 } avg_err = 0.0000")]
    #[test_case(2 => "estimator = { representation: Small, estimate: 2, size: 8 } avg_err = 0.0000")]
    #[test_case(3 => "estimator = { representation: Slice, estimate: 3, size: 24 } avg_err = 0.0000")]
    #[test_case(4 => "estimator = { representation: Slice, estimate: 4, size: 24 } avg_err = 0.0000")]
    #[test_case(8 => "estimator = { representation: Slice, estimate: 8, size: 40 } avg_err = 0.0000")]
    #[test_case(16 => "estimator = { representation: Slice, estimate: 16, size: 72 } avg_err = 0.0000")]
    #[test_case(17 => "estimator = { representation: HashSet, estimate: 17, size: 184 } avg_err = 0.0000")]
    #[test_case(28 => "estimator = { representation: HashSet, estimate: 28, size: 184 } avg_err = 0.0000")]
    #[test_case(29 => "estimator = { representation: HashSet, estimate: 29, size: 344 } avg_err = 0.0000")]
    #[test_case(56 => "estimator = { representation: HashSet, estimate: 56, size: 344 } avg_err = 0.0000")]
    #[test_case(256 => "estimator = { representation: HashSet, estimate: 256, size: 2584 } avg_err = 0.0000")]
    #[test_case(448 => "estimator = { representation: HashSet, estimate: 448, size: 2584 } avg_err = 0.0000")]
    #[test_case(896 => "estimator = { representation: HashSet, estimate: 896, size: 5144 } avg_err = 0.0000")]
    #[test_case(4096 => "estimator = { representation: HashSet, estimate: 4095, size: 40984 } avg_err = 0.0002")]
    #[test_case(8192 => "estimator = { representation: HashSet, estimate: 8191, size: 81944 } avg_err = 0.0002")]
    #[test_case(10_000 => "estimator = { representation: HashSet, estimate: 9999, size: 81944 } avg_err = 0.0002")]
    #[test_case(100_000 => "estimator = { representation: HyperLogLog, estimate: 100240, size: 196628 } avg_err = 0.0010")]
    fn test_estimator_p18_w6(n: usize) -> String {
        evaluate_cardinality_estimator(CardinalityEstimator::<18, 6>::new(), n)
    }

    fn evaluate_cardinality_estimator<const P: usize, const W: usize>(
        mut e: CardinalityEstimator<P, W>,
        n: usize,
    ) -> String {
        let mut total_relative_error: f64 = 0.0;
        for i in 0..n {
            e.insert(&i);
            let estimate = e.estimate() as f64;
            let actual = (i + 1) as f64;
            let error = estimate - actual;
            let relative_error = error.abs() / actual;
            total_relative_error += relative_error;
        }

        let avg_relative_error = total_relative_error / ((n + 1) as f64);

        format!("estimator = {:?} avg_err = {:.4}", e, avg_relative_error)
    }

    #[test_case(0, 0 => "{ representation: Small, estimate: 0, size: 8 }")]
    #[test_case(0, 1 => "{ representation: Small, estimate: 1, size: 8 }")]
    #[test_case(1, 0 => "{ representation: Small, estimate: 1, size: 8 }")]
    #[test_case(1, 1 => "{ representation: Small, estimate: 2, size: 8 }")]
    #[test_case(1, 2 => "{ representation: Slice, estimate: 3, size: 24 }")]
    #[test_case(2, 1 => "{ representation: Slice, estimate: 3, size: 24 }")]
    #[test_case(2, 2 => "{ representation: Slice, estimate: 4, size: 24 }")]
    #[test_case(2, 3 => "{ representation: Slice, estimate: 5, size: 40 }")]
    #[test_case(2, 4 => "{ representation: Slice, estimate: 6, size: 40 }")]
    #[test_case(4, 2 => "{ representation: Slice, estimate: 6, size: 40 }")]
    #[test_case(3, 2 => "{ representation: Slice, estimate: 5, size: 40 }")]
    #[test_case(3, 3 => "{ representation: Slice, estimate: 6, size: 40 }")]
    #[test_case(3, 4 => "{ representation: Slice, estimate: 7, size: 40 }")]
    #[test_case(4, 3 => "{ representation: Slice, estimate: 7, size: 40 }")]
    #[test_case(4, 4 => "{ representation: Slice, estimate: 8, size: 40 }")]
    #[test_case(4, 8 => "{ representation: Slice, estimate: 12, size: 72 }")]
    #[test_case(8, 4 => "{ representation: Slice, estimate: 12, size: 72 }")]
    #[test_case(4, 12 => "{ representation: Slice, estimate: 16, size: 72 }")]
    #[test_case(12, 4 => "{ representation: Slice, estimate: 16, size: 72 }")]
    #[test_case(4, 13 => "{ representation: HashSet, estimate: 17, size: 184 }")]
    #[test_case(13, 4 => "{ representation: HashSet, estimate: 17, size: 184 }")]
    #[test_case(1, 16 => "{ representation: HashSet, estimate: 17, size: 184 }")]
    #[test_case(16, 1 => "{ representation: HashSet, estimate: 17, size: 184 }")]
    #[test_case(17, 28 => "{ representation: HashSet, estimate: 45, size: 344 }")]
    #[test_case(28, 17 => "{ representation: HashSet, estimate: 45, size: 344 }")]
    #[test_case(4, 444 => "{ representation: HashSet, estimate: 448, size: 2584 }")]
    #[test_case(444, 4 => "{ representation: HashSet, estimate: 448, size: 2584 }")]
    #[test_case(0, 448 => "{ representation: HashSet, estimate: 448, size: 2584 }")]
    #[test_case(448, 0 => "{ representation: HashSet, estimate: 448, size: 2584 }")]
    #[test_case(1, 448 => "{ representation: HyperLogLog, estimate: 437, size: 3092 }")]
    #[test_case(448, 1 => "{ representation: HyperLogLog, estimate: 450, size: 3092 }")]
    #[test_case(512, 512 => "{ representation: HyperLogLog, estimate: 1012, size: 3092 }")]
    #[test_case(10000, 0 => "{ representation: HyperLogLog, estimate: 9908, size: 3092 }")]
    #[test_case(0, 10000 => "{ representation: HyperLogLog, estimate: 9894, size: 3092 }")]
    #[test_case(4, 10000 => "{ representation: HyperLogLog, estimate: 9896, size: 3092 }")]
    #[test_case(10000, 4 => "{ representation: HyperLogLog, estimate: 9908, size: 3092 }")]
    #[test_case(17, 10000 => "{ representation: HyperLogLog, estimate: 9913, size: 3092 }")]
    #[test_case(10000, 17 => "{ representation: HyperLogLog, estimate: 9935, size: 3092 }")]
    #[test_case(10000, 10000 => "{ representation: HyperLogLog, estimate: 19889, size: 3092 }")]
    fn test_merge(lhs_n: usize, rhs_n: usize) -> String {
        let mut lhs = CardinalityEstimator::<12, 6>::new();
        let mut buf = [0, 0, 0, 0, 0, 0, 0, 0, 1];
        for i in 0..lhs_n {
            buf[..8].copy_from_slice(&i.to_le_bytes());
            lhs.insert(&buf);
        }

        let mut rhs = CardinalityEstimator::<12, 6>::new();
        let mut buf = [0, 0, 0, 0, 0, 0, 0, 0, 2];
        for i in 0..rhs_n {
            buf[..8].copy_from_slice(&i.to_le_bytes());
            rhs.insert(&buf);
        }

        lhs.merge(&rhs);

        format!("{:?}", lhs)
    }

    #[test]
    fn test_insert() {
        // Create a new CardinalityEstimator.
        let mut e = CardinalityEstimator::<12, 6>::new();

        // Ensure initial estimate is 0.
        assert_eq!(e.estimate(), 0);

        // Insert a test item and validate estimate.
        e.insert("test item 1");
        assert_eq!(e.estimate(), 1);

        // Re-insert the same item, estimate should remain the same.
        e.insert("test item 1");
        assert_eq!(e.estimate(), 1);

        // Insert a new distinct item, estimate should increase.
        e.insert("test item 2");
        assert_eq!(e.estimate(), 2);
    }
}
