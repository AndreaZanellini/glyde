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

//! Clusters between gaps (SPEC §2.3): "clicking a gap-adjacent cluster fits
//! it to the viewport". Pure tick arithmetic on the gap list the time view
//! already holds; a view-state helper that never touches a sample.

/// The cluster of samples containing `tick`, as the ticks of its first and
/// last sample, given the axis's `gaps` (the ticks of the samples either
/// side of each, in time order) and its first and last tick.
///
/// `None` when there is no gap (the whole axis is one cluster: nothing to
/// zoom into), when `tick` lies inside a gap (no data there), or when it is
/// outside the axis. A cluster's own boundary samples belong to it.
pub fn cluster_at(
    gaps: &[(i128, i128)],
    first: i128,
    last: i128,
    tick: i128,
) -> Option<(i128, i128)> {
    if gaps.is_empty() || tick < first || tick > last {
        return None;
    }
    // The first gap that ends after `tick`: the cluster closes at its start.
    let next = gaps.partition_point(|&(_, after)| after <= tick);
    let end = match gaps.get(next) {
        Some(&(before, _)) if tick > before => return None,
        Some(&(before, _)) => before,
        None => last,
    };
    let start = match next.checked_sub(1) {
        Some(previous) => gaps[previous].1,
        None => first,
    };
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Clusters [0,10], [1010,1020], [3020,3030]; gaps (10,1010), (1020,3020).
    const GAPS: [(i128, i128); 2] = [(10, 1010), (1020, 3020)];

    fn at(tick: i128) -> Option<(i128, i128)> {
        cluster_at(&GAPS, 0, 3030, tick)
    }

    #[test]
    fn a_tick_inside_a_cluster_returns_that_cluster() {
        assert_eq!(at(5), Some((0, 10)));
        assert_eq!(at(1015), Some((1010, 1020)));
        assert_eq!(at(3025), Some((3020, 3030)));
    }

    #[test]
    fn boundary_samples_belong_to_their_cluster() {
        assert_eq!(at(0), Some((0, 10)));
        assert_eq!(at(10), Some((0, 10)));
        assert_eq!(at(1010), Some((1010, 1020)));
        assert_eq!(at(1020), Some((1010, 1020)));
        assert_eq!(at(3020), Some((3020, 3030)));
        assert_eq!(at(3030), Some((3020, 3030)));
    }

    #[test]
    fn a_tick_inside_a_gap_or_off_the_axis_is_no_cluster() {
        assert_eq!(at(11), None);
        assert_eq!(at(500), None);
        assert_eq!(at(1009), None);
        assert_eq!(at(2000), None);
        assert_eq!(at(-1), None);
        assert_eq!(at(3031), None);
    }

    #[test]
    fn without_gaps_there_is_no_cluster_to_fit() {
        assert_eq!(cluster_at(&[], 0, 100, 50), None);
    }
}
