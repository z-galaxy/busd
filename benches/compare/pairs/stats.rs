//! The statistics of a comparison: sums, means, medians and quantiles, computed as Python and
//! numpy compute them, and the percentile bootstrap of the pairs.

use std::collections::BTreeMap;

/// The mean of `values`, at least one, from their sum correctly rounded, as Python's
/// `statistics.fmean` takes it.
pub(super) fn mean(values: &[f64]) -> f64 {
    fsum(values) / values.len() as f64
}

/// The sum of `values`, correctly rounded, by Shewchuk's algorithm, as Python's `math.fsum` has it,
/// for values whose sums do not overflow.
///
/// Partial sums that do not overlap, in increasing size, hold the exact sum of the values so far:
/// each value is added to them in turn, each addition rounded, but with its rounding error kept as
/// a partial of its own. Their sum is then taken from the largest down, for as long as it is exact,
/// and rounded half to even.
pub(super) fn fsum(values: &[f64]) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    let mut next = Vec::new();
    for &value in values {
        let mut sum = value;
        for &partial in &partials {
            let (large, small) = if sum.abs() < partial.abs() {
                (partial, sum)
            } else {
                (sum, partial)
            };
            sum = large + small;
            let error = small - (sum - large);
            if error != 0.0 {
                next.push(error);
            }
        }
        if sum != 0.0 {
            next.push(sum);
        }
        std::mem::swap(&mut partials, &mut next);
        next.clear();
    }

    let Some((&largest, smaller)) = partials.split_last() else {
        return 0.0;
    };
    let mut sum = largest;
    let mut error = 0.0;
    // The partials that are not in the sum yet are those below `left`.
    let mut left = smaller.len();
    while left > 0 {
        left -= 1;
        let large = sum;
        sum = large + smaller[left];
        error = smaller[left] - (sum - large);
        if error != 0.0 {
            break;
        }
    }
    // The last addition rounded its error away. If that error was half a unit in the last place of
    // the sum, a tie, which was rounded to even, but the partials below it have its sign, the exact
    // sum lies beyond the tie, and rounds the other way.
    if left > 0
        && (error < 0.0 && smaller[left - 1] < 0.0 || error > 0.0 && smaller[left - 1] > 0.0)
    {
        let twice = error * 2.0;
        let rounded = sum + twice;
        if twice == rounded - sum {
            sum = rounded;
        }
    }

    sum
}

/// The median of `values`, at least one: the middle one once they are sorted, or the mean of the
/// two in the middle, as Python's `statistics.median` takes it.
pub(super) fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// The percentile bootstrap of the metrics of a comparison.
///
/// The indices of the pairs in its resamples depend only on the number of pairs, so they are drawn
/// once for each number of pairs and shared by the metrics that have that many, which come out the
/// same as if each drew its own. The draws are those of fastrand's generator from the seed `SEED`,
/// on a 64-bit target (fastrand draws an index from another stream on a 32-bit one).
#[derive(Default)]
pub(super) struct Bootstrap {
    /// The indices of the pairs in each resample, `RESAMPLES` of them one after another, by the
    /// number of pairs.
    drawn: BTreeMap<usize, Vec<usize>>,
}

impl Bootstrap {
    /// `100 * (candidate mean / baseline mean - 1)` in each resample of the pairs, whose baseline
    /// values are `base` and candidate values `cand`, in increasing order, and how many resamples
    /// have no such value, as their baseline mean is not positive.
    pub(super) fn percent(&mut self, base: &[f64], cand: &[f64]) -> (Vec<f64>, usize) {
        let n = base.len();
        let mut values = Vec::with_capacity(RESAMPLES);
        let mut skipped = 0;
        for resample in self.indices(n).chunks_exact(n) {
            // The ratio of the sums is that of the means.
            let base_sum: f64 = resample.iter().map(|&i| base[i]).sum();
            if base_sum <= 0.0 {
                skipped += 1;
                continue;
            }
            let cand_sum: f64 = resample.iter().map(|&i| cand[i]).sum();
            values.push(100.0 * (cand_sum / base_sum - 1.0));
        }
        values.sort_by(f64::total_cmp);

        (values, skipped)
    }

    /// The indices of the pairs in each resample of `n` pairs, one resample after another: `n`
    /// draws with replacement from `0..n` each.
    fn indices(&mut self, n: usize) -> &[usize] {
        self.drawn.entry(n).or_insert_with(|| {
            let mut rng = fastrand::Rng::with_seed(SEED);
            (0..RESAMPLES * n).map(|_| rng.usize(..n)).collect()
        })
    }
}

/// How many resamples the bootstrap draws.
pub(super) const RESAMPLES: usize = 10_000;

/// The seed of the bootstrap's draws, which is always the same, so that a comparison can be made
/// again exactly.
pub(super) const SEED: u64 = 1;

/// The quantile `q` of `sorted`, values in increasing order, at least one, by linear interpolation
/// between the order statistics, as numpy's default has it.
pub(super) fn quantile(sorted: &[f64], q: f64) -> f64 {
    let last = sorted.len() - 1;
    let position = q * last as f64;
    let below = position.floor() as usize;
    let above = (below + 1).min(last);

    sorted[below] + (sorted[above] - sorted[below]) * (position - below as f64)
}
