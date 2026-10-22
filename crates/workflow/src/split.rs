// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Splits a list of tests across parallel nodes by recorded timing.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};

/// Returns the items that node `index` of `total` should run.
///
/// Every node computes the same assignment from the same inputs, so the
/// shares never overlap and together cover every item. Items with a
/// known timing keep it. Items without one get the median of the known
/// timings (1.0 when none are known). Zero, negative and non-finite
/// timings count as unknown, so a file of zeros balances by count. Items are placed longest first, ties by name, each
/// on the node with the smallest total so far (ties to the lowest node).
/// The result keeps the input order. Duplicate items are separate items.
/// `total == 0` or `index >= total` gives an empty list.
///
/// Every item lands on one of the first `items.len()` nodes, so the work
/// is done for at most that many nodes (at least one) whatever `total`
/// is, and a node past them gets nothing.
#[must_use]
pub fn split<'a>(
    items: &[&'a str],
    timings: &BTreeMap<String, f64>,
    index: u32,
    total: u32,
) -> Vec<&'a str> {
    let nodes = (total as usize).min(items.len().max(1));
    if total == 0 || index as usize >= nodes {
        return Vec::new();
    }
    let known: Vec<Option<f64>> = items
        .iter()
        .map(|item| {
            timings
                .get(*item)
                .copied()
                .filter(|t| t.is_finite() && *t > 0.0)
        })
        .collect();
    let fallback = median(&known);
    let weights: Vec<f64> = known.iter().map(|t| t.unwrap_or(fallback)).collect();

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        weights[b]
            .total_cmp(&weights[a])
            .then_with(|| items[a].cmp(items[b]))
            .then_with(|| a.cmp(&b))
    });

    // Smallest load first, ties to the lowest node.
    let mut heap: BinaryHeap<Reverse<(Load, usize)>> =
        (0..nodes).map(|node| Reverse((Load(0.0), node))).collect();
    let mut mine = vec![false; items.len()];
    for i in order {
        let Some(Reverse((Load(sum), node))) = heap.pop() else {
            break;
        };
        heap.push(Reverse((Load(sum + weights[i]), node)));
        mine[i] = node == index as usize;
    }
    items
        .iter()
        .zip(mine)
        .filter_map(|(item, keep)| keep.then_some(*item))
        .collect()
}

/// A node's total time so far. Loads are sums of finite, positive
/// weights, so the total order never has to rank a NaN.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Load(f64);

impl Eq for Load {}

impl PartialOrd for Load {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Load {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Median of the known timings, 1.0 when there are none.
fn median(known: &[Option<f64>]) -> f64 {
    let mut values: Vec<f64> = known.iter().flatten().copied().collect();
    if values.is_empty() {
        return 1.0;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_partition_the_items() {
        let items = ["a", "b", "c", "d", "e"];
        let timings = BTreeMap::from([
            ("a".into(), 10.0),
            ("b".into(), 1.0),
            ("c".into(), 1.0),
            ("d".into(), 8.0),
        ]);
        let all: Vec<Vec<&str>> = (0..2).map(|i| split(&items, &timings, i, 2)).collect();
        // e gets the median 4.5; order a10 d8 e4.5 b1 c1 -> node0 a,b,c (12), node1 d,e (12.5)
        assert_eq!(all[0], ["a", "b", "c"]);
        assert_eq!(all[1], ["d", "e"]);
    }

    #[test]
    fn no_timings_balances_by_count() {
        let items = ["a", "b", "c", "d"];
        let lens: Vec<usize> = (0..3)
            .map(|i| split(&items, &BTreeMap::new(), i, 3).len())
            .collect();
        assert_eq!(lens, [2, 1, 1]);
    }

    #[test]
    fn edge_cases_do_not_panic() {
        let items = ["a", "b"];
        let none = BTreeMap::new();
        assert_eq!(split(&items, &none, 0, 0).len(), 0);
        assert_eq!(split(&items, &none, 2, 2).len(), 0);
        assert_eq!(split(&[], &none, 0, 3).len(), 0);

        // Duplicates are separate items and each stays where it was placed.
        let dups = ["a", "a", "a"];
        let n: usize = (0..2).map(|i| split(&dups, &none, i, 2).len()).sum();
        assert_eq!(n, 3);

        // Bad timings count as unknown: all weigh 1.0, so counts balance.
        let bad = BTreeMap::from([
            ("a".into(), f64::NAN),
            ("b".into(), -5.0),
            ("c".into(), f64::INFINITY),
        ]);
        let items = ["a", "b", "c", "d"];
        let lens: Vec<usize> = (0..2).map(|i| split(&items, &bad, i, 2).len()).collect();
        assert_eq!(lens, [2, 2]);
    }

    #[test]
    fn all_zero_timings_count_as_unknown() {
        let items = ["a", "b", "c", "d"];
        let zeros: BTreeMap<String, f64> = items.iter().map(|i| ((*i).into(), 0.0)).collect();
        let lens: Vec<usize> = (0..2).map(|i| split(&items, &zeros, i, 2).len()).collect();
        assert_eq!(lens, [2, 2]);
    }

    #[test]
    fn a_big_split_is_fast() {
        let names: Vec<String> = (0..200_000).map(|i| format!("t{i}")).collect();
        let items: Vec<&str> = names.iter().map(String::as_str).collect();
        let start = std::time::Instant::now();
        let mine = split(&items, &BTreeMap::new(), 7, 200_000);
        assert_eq!(mine.len(), 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_huge_total_costs_no_memory() {
        let items = ["a", "b", "c"];
        let none = BTreeMap::new();
        // `vec![0.0; u32::MAX]` would be 32 GiB.
        assert_eq!(split(&items, &none, 0, u32::MAX), ["a"]);
        assert_eq!(split(&items, &none, 2, u32::MAX), ["c"]);
        assert_eq!(split(&items, &none, 3, u32::MAX).len(), 0);
        assert_eq!(split(&items, &none, u32::MAX - 1, u32::MAX).len(), 0);
        assert_eq!(split(&[], &none, 0, u32::MAX).len(), 0);
        // Clamping keeps the assignment a larger total would give.
        for i in 0..3 {
            assert_eq!(split(&items, &none, i, 3), split(&items, &none, i, 1000));
        }
    }
}
