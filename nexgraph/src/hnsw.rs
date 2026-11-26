//! Hierarchical Navigable Small World index (Malkov & Yashunin, 2016),
//! in-memory, deterministic (seeded level generator), append-only.
//!
//! Each indexed point carries `(id, from_ts)` so the store can tell which
//! entity *version* a hit belongs to and filter by MVCC visibility.

use crate::model::Metric;
use crate::value::Id;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

#[derive(Clone, Copy, PartialEq)]
struct Dist(f32);
impl Eq for Dist {}
impl PartialOrd for Dist {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Dist {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

struct Point {
    id: Id,
    from: u64,
    vec: Vec<f32>,
    links: Vec<Vec<u32>>, // links[level]
}

pub struct Hnsw {
    dim: usize,
    metric: Metric,
    m: usize,
    ef_construction: usize,
    points: Vec<Point>,
    entry: Option<u32>,
    max_level: usize,
    rng: u64,
}

/// A search hit.
#[derive(Clone, Debug)]
pub struct Hit {
    pub id: Id,
    pub from: u64,
    /// Distance under the index metric (cosine: 1 - cos; L2: squared).
    pub dist: f32,
}

impl Hnsw {
    pub fn new(dim: usize, metric: Metric) -> Self {
        Hnsw { dim, metric, m: 16, ef_construction: 100, points: Vec::new(), entry: None, max_level: 0, rng: 0x9E37_79B9_7F4A_7C15 }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }
    pub fn metric(&self) -> Metric {
        self.metric
    }
    pub fn len(&self) -> usize {
        self.points.len()
    }
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    fn next_rand(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((r >> 11) as f64) / ((1u64 << 53) as f64)
    }

    fn random_level(&mut self) -> usize {
        let ml = 1.0 / (self.m as f64).ln();
        let u = self.next_rand().max(1e-12);
        ((-u.ln() * ml).floor() as usize).min(16)
    }

    fn prep(&self, v: &[f32]) -> Vec<f32> {
        match self.metric {
            Metric::Cosine => {
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if n > 0.0 {
                    v.iter().map(|x| x / n).collect()
                } else {
                    v.to_vec()
                }
            }
            Metric::L2 => v.to_vec(),
        }
    }

    fn dist(&self, a: &[f32], b: &[f32]) -> f32 {
        match self.metric {
            Metric::Cosine => 1.0 - a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>(),
            Metric::L2 => a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum(),
        }
    }

    /// Distance between a raw (unnormalised) query and a raw vector, matching
    /// what `search` reports. Used for exact re-ranking.
    pub fn raw_distance(&self, a: &[f32], b: &[f32]) -> f32 {
        let (pa, pb) = (self.prep(a), self.prep(b));
        self.dist(&pa, &pb)
    }

    fn search_layer(&self, q: &[f32], entries: &[u32], ef: usize, level: usize) -> Vec<(Dist, u32)> {
        let mut visited: HashSet<u32> = entries.iter().copied().collect();
        let mut cand: BinaryHeap<std::cmp::Reverse<(Dist, u32)>> = BinaryHeap::new();
        let mut best: BinaryHeap<(Dist, u32)> = BinaryHeap::new();
        for &e in entries {
            let d = Dist(self.dist(q, &self.points[e as usize].vec));
            cand.push(std::cmp::Reverse((d, e)));
            best.push((d, e));
        }
        while let Some(std::cmp::Reverse((d, c))) = cand.pop() {
            if let Some(&(worst, _)) = best.peek() {
                if d > worst && best.len() >= ef {
                    break;
                }
            }
            if let Some(links) = self.points[c as usize].links.get(level) {
                for &n in links {
                    if !visited.insert(n) {
                        continue;
                    }
                    let dn = Dist(self.dist(q, &self.points[n as usize].vec));
                    let worst = best.peek().map(|x| x.0);
                    if best.len() < ef || worst.map_or(true, |w| dn < w) {
                        cand.push(std::cmp::Reverse((dn, n)));
                        best.push((dn, n));
                        if best.len() > ef {
                            best.pop();
                        }
                    }
                }
            }
        }
        let mut out = best.into_vec();
        out.sort();
        out
    }

    pub fn insert(&mut self, id: Id, from: u64, v: &[f32]) {
        debug_assert_eq!(v.len(), self.dim);
        let vec = self.prep(v);
        let level = self.random_level();
        let idx = self.points.len() as u32;
        self.points.push(Point { id, from, vec: vec.clone(), links: vec![Vec::new(); level + 1] });
        let Some(mut ep) = self.entry else {
            self.entry = Some(idx);
            self.max_level = level;
            return;
        };
        let mut l = self.max_level;
        while l > level {
            let r = self.search_layer(&vec, &[ep], 1, l);
            ep = r[0].1;
            l -= 1;
        }
        let mut eps = vec![ep];
        for lc in (0..=level.min(self.max_level)).rev() {
            let found = self.search_layer(&vec, &eps, self.ef_construction, lc);
            let mmax = if lc == 0 { self.m * 2 } else { self.m };
            let chosen: Vec<u32> = found.iter().take(self.m).map(|x| x.1).collect();
            self.points[idx as usize].links[lc] = chosen.clone();
            for &n in &chosen {
                let nl = &mut self.points[n as usize].links[lc];
                nl.push(idx);
                if nl.len() > mmax {
                    let base = self.points[n as usize].vec.clone();
                    let mut scored: Vec<(Dist, u32)> = self.points[n as usize].links[lc]
                        .iter()
                        .map(|&x| (Dist(self.dist(&base, &self.points[x as usize].vec)), x))
                        .collect();
                    scored.sort();
                    scored.truncate(mmax);
                    self.points[n as usize].links[lc] = scored.into_iter().map(|x| x.1).collect();
                }
            }
            eps = found.into_iter().map(|x| x.1).collect();
        }
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(idx);
        }
    }

    /// Approximate nearest points, closest first. Returns up to `max(ef, k)`
    /// candidates so callers can post-filter (e.g. by MVCC visibility) and
    /// then truncate to `k`.
    pub fn search(&self, query: &[f32], k: usize, ef: usize) -> Vec<Hit> {
        let Some(mut ep) = self.entry else { return Vec::new() };
        if query.len() != self.dim {
            return Vec::new();
        }
        let q = self.prep(query);
        let mut l = self.max_level;
        while l > 0 {
            let r = self.search_layer(&q, &[ep], 1, l);
            ep = r[0].1;
            l -= 1;
        }
        let found = self.search_layer(&q, &[ep], ef.max(k), 0);
        found
            .into_iter()
            .map(|(d, i)| {
                let p = &self.points[i as usize];
                Hit { id: p.id, from: p.from, dist: d.0 }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32) / (1u64 << 31) as f32 - 0.5
    }

    #[test]
    fn recall_against_brute_force() {
        let (n, dim, k) = (1500usize, 16usize, 10usize);
        let mut s = 7u64;
        let data: Vec<Vec<f32>> = (0..n).map(|_| (0..dim).map(|_| lcg(&mut s)).collect()).collect();
        let mut h = Hnsw::new(dim, Metric::L2);
        for (i, v) in data.iter().enumerate() {
            h.insert(i as Id, 1, v);
        }
        let mut hits = 0;
        let queries = 40;
        for _ in 0..queries {
            let q: Vec<f32> = (0..dim).map(|_| lcg(&mut s)).collect();
            let mut exact: Vec<(f32, usize)> = data
                .iter()
                .enumerate()
                .map(|(i, v)| (v.iter().zip(&q).map(|(a, b)| (a - b) * (a - b)).sum(), i))
                .collect();
            exact.sort_by(|a, b| a.0.total_cmp(&b.0));
            let truth: HashSet<u64> = exact.iter().take(k).map(|x| x.1 as u64).collect();
            let got = h.search(&q, k, 80);
            hits += got.iter().filter(|g| truth.contains(&g.id)).count();
        }
        let recall = hits as f64 / (queries * k) as f64;
        assert!(recall > 0.95, "recall {recall}");
    }
}
