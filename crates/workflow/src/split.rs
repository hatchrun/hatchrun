// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Splits a list of tests across parallel nodes by recorded timing.

use std::collections::BTreeMap;

/// Returns the items that node `index` of `total` should run.
///
/// Every node computes the same assignment from the same inputs, so the
/// shares never overlap and together cover every item. Items with a
/// known timing keep it. Items without one get the median of the known
/// timings (1.0 when none are known). Negative or non-finite timings
/// count as unknown. Items are placed longest first, ties by name, each
/// on the node with the smallest total so far (ties to the lowest node).
/// The result keeps the input order. Duplicate items are separate items.
/// `total == 0` or `index >= total` gives an empty list.
#[must_use]
pub fn split<'a>(
    items: &[&'a str],
    timings: &BTreeMap<String, f64>,
    index: u32,
    total: u32,
) -> Vec<&'a str> {
    if total == 0 || index >= total {
        return Vec::new();
    }
    let known: Vec<Option<f64>> = items
        .iter()
        .map(|item| {
            timings
                .get(*item)
                .copied()
                .filter(|t| t.is_finite() && *t >= 0.0)
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

    let mut sums = vec![0.0_f64; total as usize];
    let mut mine = vec![false; items.len()];
    for i in order {
        let mut best = 0;
        for (node, sum) in sums.iter().enumerate() {
            if sum.total_cmp(&sums[best]).is_lt() {
                best = node;
            }
        }
        sums[best] += weights[i];
        mine[i] = best == index as usize;
    }
    items
        .iter()
        .zip(mine)
        .filter_map(|(item, keep)| keep.then_some(*item))
        .collect()
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
}
