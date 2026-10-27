//! Pulsar: make a frame cost O(pixels), not O(samples).
//!
//! Upstream's line loader re-reads every sample of every series from the store,
//! re-sorts it and re-aggregates it on every frame, which is 2/3 of a frame on a
//! 176 MB flight log. Nothing of that depends on the frame unless the data does.
//!
//! So the loader queries the whole data range (its result, and Rerun's hash of
//! it, stay the same while the user zooms), and this module keeps the prepared
//! points across frames, keyed on that hash. Per frame, the loader slices the
//! visible window by binary search and, when a window holds many samples per
//! pixel, computes the aggregator's per-pixel output from a min/max pyramid level
//! instead of from the raw samples. Min and max nest, so Min, Max, MinMax and
//! MinMaxAverage come out as from the raw samples, up to bucket edges (buckets are
//! runs of samples, not spans of time); Average uses the bucket's exact mean.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use re_sdk_types::blueprint::components::VisualizerInstructionId;
use re_sdk_types::components::AggregationPolicy;
use re_sdk_types::ComponentIdentifier;

use crate::PlotPoint;

/// Everything about an instruction's series that does not depend on the frame.
pub(crate) struct Prepared {
    pub points_per_series: Vec<Vec<PlotPoint>>,
    /// `None` where a series has discontinuities (clears): those are sliced, not decimated.
    pub pyramids: Vec<Option<Pyramid>>,
    pub names: Vec<String>,
    pub units: Vec<Option<String>>,
    pub visibility: Vec<bool>,
    pub has_variances: bool,
    pub aggregator: AggregationPolicy,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub instruction: VisualizerInstructionId,
    pub value_component: ComponentIdentifier,
    /// Rerun's hash of the query result: chunks, query range, overrides and defaults.
    pub results: u64,
    /// The recursive clears that apply, which the result hash does not cover.
    pub clears: u64,
}

const MAX_ENTRIES: usize = 256;

static CACHE: LazyLock<Mutex<(u64, HashMap<Key, (u64, Arc<Prepared>)>)>> =
    LazyLock::new(|| Mutex::new((0, HashMap::new())));

pub(crate) fn get(key: &Key) -> Option<Arc<Prepared>> {
    let mut guard = CACHE.lock().ok()?;
    let (clock, map) = &mut *guard;
    *clock += 1;
    let now = *clock;
    map.get_mut(key).map(|(used, prepared)| {
        *used = now;
        Arc::clone(prepared)
    })
}

pub(crate) fn insert(key: Key, prepared: Arc<Prepared>) {
    let Ok(mut guard) = CACHE.lock() else {
        return;
    };
    let (clock, map) = &mut *guard;
    *clock += 1;
    if map.len() >= MAX_ENTRIES
        && let Some(oldest) = map.iter().min_by_key(|(_, (used, _))| *used).map(|(k, _)| *k)
    {
        map.remove(&oldest);
    }
    map.insert(key, (*clock, prepared));
}

/// Index range `[lo, hi)` of the points within `[min, max]`, plus one point on
/// each side, so lines run on to the plot's edges.
pub(crate) fn window(points: &[PlotPoint], min: i64, max: i64) -> (usize, usize) {
    let lo = points.partition_point(|p| p.time < min).saturating_sub(1);
    let hi = (points.partition_point(|p| p.time <= max) + 1).min(points.len());
    (lo, hi.max(lo))
}

/// Min/max of consecutive runs of samples, at sizes 2, 4, 8, ...
pub(crate) struct Pyramid {
    /// `levels[k]` has one bucket per `2^(k+1)` samples.
    levels: Vec<Vec<Bucket>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    min: u32,
    max: u32,
    sum: f64,
}

impl Pyramid {
    pub fn build(points: &[PlotPoint]) -> Self {
        let mut levels: Vec<Vec<Bucket>> = Vec::new();
        let leaf = |i: usize| Bucket {
            min: i as u32,
            max: i as u32,
            sum: points[i].value,
        };
        let merge = |a: Bucket, b: Bucket| Bucket {
            min: if points[b.min as usize].value < points[a.min as usize].value { b.min } else { a.min },
            max: if points[b.max as usize].value > points[a.max as usize].value { b.max } else { a.max },
            sum: a.sum + b.sum,
        };
        let mut below: Vec<Bucket> = (0..points.len()).map(leaf).collect();
        while below.len() > 1 {
            let level: Vec<Bucket> = below
                .chunks(2)
                .map(|pair| if pair.len() == 2 { merge(pair[0], pair[1]) } else { pair[0] })
                .collect();
            levels.push(level.clone());
            below = level;
        }
        Self { levels }
    }

    /// The aggregator's own output for `points[lo..hi]`, computed from the pyramid at the level
    /// whose buckets hold at most `samples_per_pixel` samples, so about one bucket per pixel:
    /// per bucket the minimum and maximum (`MinMax`), their midpoint (`MinMaxAverage`), one
    /// of them (`Min`, `Max`), or the mean (`Average`). `None` when no level fits; the caller
    /// then aggregates the raw slice as upstream does.
    pub fn aggregate(
        &self,
        points: &[PlotPoint],
        lo: usize,
        hi: usize,
        samples_per_pixel: usize,
        aggregator: AggregationPolicy,
    ) -> Option<Vec<PlotPoint>> {
        let k = (0..self.levels.len())
            .rev()
            .find(|&k| (2usize << k) <= samples_per_pixel)?;
        let size = 2usize << k;
        let level = &self.levels[k];
        let (first, last) = (lo / size, hi.div_ceil(size).min(level.len()));
        let mut out = Vec::with_capacity((last - first) * 2 + 2);
        if lo % size != 0 {
            out.push(points[lo].clone()); // keep the line's start exact
        }
        for (i, b) in level[first..last].iter().enumerate() {
            let (min, max) = (&points[b.min as usize], &points[b.max as usize]);
            let (a, z) = if b.min <= b.max { (min, max) } else { (max, min) };
            match aggregator {
                AggregationPolicy::Off | AggregationPolicy::MinMax => {
                    out.push(a.clone());
                    if b.min != b.max {
                        out.push(z.clone());
                    }
                }
                AggregationPolicy::MinMaxAverage => {
                    let mut p = a.clone();
                    p.value = (min.value + max.value) * 0.5;
                    p.variance = (min.variance + max.variance) * 0.5;
                    p.attrs.radius_ui = (min.attrs.radius_ui + max.attrs.radius_ui) * 0.5;
                    out.push(p);
                }
                AggregationPolicy::Min => out.push(min.clone()),
                AggregationPolicy::Max => out.push(max.clone()),
                AggregationPolicy::Average => {
                    let start = (first + i) * size;
                    let end = (start + size).min(points.len());
                    let mut p = points[(start + end) / 2].clone();
                    p.value = b.sum / (end - start).max(1) as f64;
                    out.push(p);
                }
            }
        }
        if hi < points.len() && hi % size != 0 {
            out.push(points[hi - 1].clone()); // and its end
        }
        Some(out)
    }
}
