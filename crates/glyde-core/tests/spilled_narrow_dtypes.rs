// Copyright 2026 The Glyde Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Issue #155's prerequisite: a Parquet file too large for memory spills its
//! columns in their own dtypes — `i8` … `u64`, `f32` — never upcast (SPEC
//! §1.4, Golden Rule 1). A spilled column of each must be the same data as
//! the in-memory column it would have been: equal to it, and read back the
//! same way by every accessor the views, the pyramid and the readout use.

use glyde_core::index::spill::{SpillSet, SpillVec, SpillVecWriter};
use glyde_core::series::{SampleSource, SeriesValues, SpilledValues};

fn spill<T: bytemuck::Pod>(set: &SpillSet, stem: &str, values: &[T]) -> SpillVec<T> {
    let mut writer = SpillVecWriter::create(set, stem).expect("create a spill file");
    writer
        .extend_from_slice(values)
        .expect("write the spill file");
    writer.finish().expect("finish the spill file")
}

/// Every sample `values` hands out in chunks, promoted to `f64`.
fn sampled(values: &SeriesValues) -> Option<Vec<f64>> {
    let source = values.sample_source()?;
    let mut samples = Vec::new();
    source
        .visit_sample_chunks(0..source.sample_count(), &mut |chunk| {
            samples.extend_from_slice(chunk);
            Ok(())
        })
        .expect("an in-memory or spilled column reads back");
    Some(samples)
}

fn bits(values: Option<Vec<f64>>) -> Option<Vec<u64>> {
    values.map(|values| values.into_iter().map(f64::to_bits).collect())
}

/// `spilled` reads back exactly as `memory` does through every accessor.
fn assert_same_data(memory: &SeriesValues, spilled: &SeriesValues, what: &str) {
    assert_eq!(spilled.dtype(), memory.dtype(), "{what}: dtype");
    assert_eq!(spilled.len(), memory.len(), "{what}: len");
    assert_eq!(
        spilled.is_constant(),
        memory.is_constant(),
        "{what}: constant"
    );
    assert_eq!(
        spilled.as_f64_slice().is_some(),
        memory.as_f64_slice().is_some(),
        "{what}: zero-copy f64"
    );
    assert_eq!(
        bits(spilled.to_f64_vec()),
        bits(memory.to_f64_vec()),
        "{what}: to_f64_vec"
    );
    assert_eq!(
        bits(sampled(spilled)),
        bits(sampled(memory)),
        "{what}: sample_source"
    );
    for index in 0..=memory.len() {
        assert_eq!(
            spilled.f64_at(index).map(f64::to_bits),
            memory.f64_at(index).map(f64::to_bits),
            "{what}: f64_at({index})"
        );
        assert_eq!(
            spilled.f64_at_checked(index).map(f64::to_bits),
            memory.f64_at_checked(index).map(f64::to_bits),
            "{what}: f64_at_checked({index})"
        );
        assert_eq!(
            spilled.display_at(index),
            memory.display_at(index),
            "{what}: display_at({index})"
        );
    }
}

#[test]
fn every_narrow_dtype_spills_as_the_same_data() {
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");

    let i8s = [i8::MIN, -1, 0, 1, 7, i8::MAX];
    let i16s = [i16::MIN, -300, 0, 300, i16::MAX];
    let i32s = [i32::MIN, -70_000, 0, 70_000, i32::MAX];
    let u8s = [0u8, 1, 128, u8::MAX];
    let u16s = [0u16, 1, 40_000, u16::MAX];
    let u32s = [0u32, 1, 3_000_000_000, u32::MAX];
    // Beyond ±2⁵³ too, which `f64` cannot hold exactly (SPEC §1.4).
    let u64s = [0u64, 1, 1 << 53, (1 << 53) + 1, u64::MAX];
    let f32s = [
        0.0f32,
        -0.0,
        1.5,
        -2.25,
        f32::MIN_POSITIVE,
        f32::MAX,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ];
    let cases = [
        (
            "i8",
            SeriesValues::I8(i8s.to_vec()),
            SpilledValues::I8(spill(&set, "i8", &i8s)),
        ),
        (
            "i16",
            SeriesValues::I16(i16s.to_vec()),
            SpilledValues::I16(spill(&set, "i16", &i16s)),
        ),
        (
            "i32",
            SeriesValues::I32(i32s.to_vec()),
            SpilledValues::I32(spill(&set, "i32", &i32s)),
        ),
        (
            "u8",
            SeriesValues::U8(u8s.to_vec()),
            SpilledValues::U8(spill(&set, "u8", &u8s)),
        ),
        (
            "u16",
            SeriesValues::U16(u16s.to_vec()),
            SpilledValues::U16(spill(&set, "u16", &u16s)),
        ),
        (
            "u32",
            SeriesValues::U32(u32s.to_vec()),
            SpilledValues::U32(spill(&set, "u32", &u32s)),
        ),
        (
            "u64",
            SeriesValues::U64(u64s.to_vec()),
            SpilledValues::U64(spill(&set, "u64", &u64s)),
        ),
        (
            "f32",
            SeriesValues::F32(f32s.to_vec()),
            SpilledValues::F32(spill(&set, "f32", &f32s)),
        ),
    ];
    for (what, memory, spilled) in cases {
        let spilled = SeriesValues::Spilled(spilled);
        assert_eq!(spilled, memory, "{what}: spilled == in memory");
        assert_eq!(memory, spilled, "{what}: in memory == spilled");
        assert_same_data(&memory, &spilled, what);
    }
}

// A float column's NaN stays a NaN, and a constant column stays constant,
// whichever storage holds it. (A column holding a NaN is not equal to
// itself, in memory or spilled alike, so only the accessors are compared.)
#[test]
fn a_spilled_f32_keeps_its_nan_and_constant_columns_stay_constant() {
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");
    let with_nan = [1.0f32, f32::NAN, 3.0];
    assert_same_data(
        &SeriesValues::F32(with_nan.to_vec()),
        &SeriesValues::Spilled(SpilledValues::F32(spill(&set, "nan", &with_nan))),
        "f32 with NaN",
    );
    let constant = [5i16; 4];
    let spilled = SeriesValues::Spilled(SpilledValues::I16(spill(&set, "constant", &constant)));
    assert!(spilled.is_constant());
    assert_same_data(
        &SeriesValues::I16(constant.to_vec()),
        &spilled,
        "constant i16",
    );
}

// Same dtype, same samples: equal. Same samples, another dtype: not equal —
// an `i32` column is never the `i64` one with the same numbers.
#[test]
fn spilled_columns_compare_by_dtype_and_value() {
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");
    let as_i32 = SeriesValues::Spilled(SpilledValues::I32(spill(&set, "a", &[1i32, 2, 3])));
    let again = SeriesValues::Spilled(SpilledValues::I32(spill(&set, "b", &[1i32, 2, 3])));
    let other = SeriesValues::Spilled(SpilledValues::I32(spill(&set, "c", &[1i32, 2, 4])));
    assert_eq!(as_i32, again);
    assert_ne!(as_i32, other);
    assert_ne!(as_i32, SeriesValues::I64(vec![1, 2, 3]));
    assert_ne!(
        as_i32,
        SeriesValues::Spilled(SpilledValues::I64(spill(&set, "d", &[1i64, 2, 3])))
    );
}
