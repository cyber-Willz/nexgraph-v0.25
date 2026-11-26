//! `swarm.*`: detection and characterisation of coordinated groups in track data (defensive analytics: it reports
//! which tracks move together and how confident that is; it does not classify intent or support engagement).
//!
//! `swarm.detect(label, idKey, timeKey, xKey, yKey, zKey, win[, opts])`
//!
//! One edge per observation (`id, t, x, y, z`, Cartesian, one length unit). Time is cut into windows of `win` time units
//! (`floor(t / win)`). Per track and window, with at least `min_samples` samples at more than one time, the position is the
//! sample mean and the velocity the least-squares slope of position on time (this averages out position jitter, which a
//! two-point difference does not). Two tracks are **linked in a window** when both have a velocity, are within `eps`, their
//! velocities have cosine >= `cos_min`, and the slower speed is at least `speed_tol` of the faster. A pair is
//! **persistent** when it was co-present (both with a velocity) in at least `min_windows` windows and linked in at least a
//! fraction `persist` of them. Groups are the connected components of persistent pairs with at least `min_size` tracks.
//!
//! Per group: `heading_coherence` is the mean over windows of the length of the mean unit velocity (1 = parallel, about
//! 1/sqrt(n) for unrelated headings). Two Monte Carlo p-values (`perm` draws, deterministic `seed`):
//! * `p_coherence`: against **independent random headings** (uniform on the sphere, or on the circle with `dim=2`) for the
//!   same group size in the same windows: "is this more aligned than unrelated tracks would be?";
//! * `p_background`: against groups of the same size drawn at random from the tracks actually present in those windows:
//!   "is this group more aligned than the rest of the airspace?".
//! `verdict`: `coordinated` when `p_coherence <= p_max`, `heading_coherence >= coh_min` and `p_background <= p_max`;
//! `coordinated (whole scene)` when the first two hold but `p_background` does not, i.e. everything visible moves
//! together, so the group cannot be told apart from the background (the one-group-is-all-traffic case); otherwise
//! `co-located`: close together without shared motion, which is what crowds, flocks of birds and busy airspace look like
//! to a proximity-only detector. The tests do not know about intent: a legitimate formation is `coordinated` too.
//!
//! Options (`key=value;...`): `eps` 20, `cos_min` 0.8, `speed_tol` 0.5, `persist` 0.6, `min_windows` 3, `min_size` 3,
//! `min_samples` 3, `dim` 3 (2 ignores z), `perm` 1000 (0 turns the tests off), `seed` 1, `p_max` 0.01, `coh_min` 0.6, `max_tracks` 2000 per window,
//! `filter` (one condition). Cost is O(tracks^2) per window.
//!
//! Output: a `(summary)` row, then one row per group: `group, size, members, windows, first_window, last_window,
//! persistence, mean_pair_dist, heading_coherence, speed_mean, speed_cv, p_coherence, p_background, verdict`.
use crate::alm::select;
use crate::error::Result;
use crate::killchain::{f, fo, group_of, num, opt_str, require_key};
use crate::netgraph::*;
use crate::store::View;
use crate::value::Value;
use std::collections::BTreeMap;

pub fn call(view: &View, name: &str, a: &[Value]) -> Result<Rows> {
    match name {
        "swarm.detect" => detect(view, a),
        "swarm.scan" => scan(view, a),
        o => bad(format!("unknown procedure {o}; the swarm procedures are swarm.detect, swarm.scan")),
    }
}

struct Opts {
    eps: f64,
    cos_min: f64,
    speed_tol: f64,
    persist: f64,
    min_windows: usize,
    min_size: usize,
    min_samples: usize,
    dim: usize,
    perm: usize,
    seed: u64,
    p_max: f64,
    coh_min: f64,
    max_tracks: usize,
    filter: Option<String>,
}

fn parse_opts(s: Option<&str>) -> Result<Opts> {
    let mut o = Opts { eps: 20.0, cos_min: 0.8, speed_tol: 0.5, persist: 0.6, min_windows: 3, min_size: 3, min_samples: 3, dim: 3, perm: 1000, seed: 1, p_max: 0.01, coh_min: 0.6, max_tracks: 2000, filter: None };
    for kv in s.unwrap_or("").split(';').map(str::trim).filter(|x| !x.is_empty()) {
        let (k, v) = kv.split_once('=').ok_or_else(|| crate::error::Error::Query(format!("option '{kv}' is not key=value")))?;
        let (k, v) = (k.trim(), v.trim());
        let fl = |name: &str| -> Result<f64> { v.parse::<f64>().ok().filter(|x| x.is_finite()).ok_or_else(|| crate::error::Error::Query(format!("{name} must be a number, not '{v}'"))) };
        let un = |name: &str| -> Result<usize> { v.parse::<usize>().map_err(|_| crate::error::Error::Query(format!("{name} must be a whole number >= 0, not '{v}'"))) };
        match k {
            "eps" => o.eps = fl(k)?,
            "cos_min" => o.cos_min = fl(k)?,
            "speed_tol" => o.speed_tol = fl(k)?,
            "persist" => o.persist = fl(k)?,
            "min_windows" => o.min_windows = un(k)?.max(1),
            "min_size" => o.min_size = un(k)?.max(2),
            "min_samples" => o.min_samples = un(k)?.max(2),
            "dim" => {
                o.dim = un(k)?;
                if o.dim != 2 && o.dim != 3 {
                    return bad("dim must be 2 or 3");
                }
            }
            "perm" => o.perm = un(k)?,
            "seed" => o.seed = v.parse::<u64>().map_err(|_| crate::error::Error::Query("seed must be a whole number".into()))?,
            "p_max" => o.p_max = fl(k)?,
            "coh_min" => o.coh_min = fl(k)?,
            "max_tracks" => o.max_tracks = un(k)?.max(2),
            "filter" => o.filter = Some(v.to_string()),
            other => return bad(format!("unknown option '{other}' (eps, cos_min, speed_tol, persist, min_windows, min_size, min_samples, dim, perm, seed, p_max, coh_min, max_tracks, filter)")),
        }
    }
    if o.eps <= 0.0 {
        return bad("eps must be > 0");
    }
    Ok(o)
}

/// splitmix64: tiny, fixed and documented so the Monte Carlo p-value can be replayed outside the engine.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// uniform in [0, 1) from the top 53 bits
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / 9007199254740992.0
    }
    /// uniform direction on the sphere (dim 3: z uniform in [-1, 1], azimuth uniform) or the circle (dim 2)
    fn direction(&mut self, dim: usize) -> [f64; 3] {
        if dim == 2 {
            let a = 2.0 * std::f64::consts::PI * self.unit();
            [a.cos(), a.sin(), 0.0]
        } else {
            let z = 2.0 * self.unit() - 1.0;
            let a = 2.0 * std::f64::consts::PI * self.unit();
            let r = (1.0 - z * z).max(0.0).sqrt();
            [r * a.cos(), r * a.sin(), z]
        }
    }
}

#[derive(Clone)]
struct Tw {
    id: usize,
    p: [f64; 3],
    v: [f64; 3],
    speed: f64,
}

fn norm(a: [f64; 3]) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}
fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    norm([a[0] - b[0], a[1] - b[1], a[2] - b[2]])
}

fn find(p: &mut Vec<usize>, x: usize) -> usize {
    let mut r = x;
    while p[r] != r {
        r = p[r];
    }
    let mut c = x;
    while p[c] != r {
        let n = p[c];
        p[c] = r;
        c = n;
    }
    r
}

fn detect(view: &View, a: &[Value]) -> Result<Rows> {
    let label = need_str(a, 0, "label")?;
    let (idk, tk, xk, yk, zk) = (need_str(a, 1, "idKey")?, need_str(a, 2, "timeKey")?, need_str(a, 3, "xKey")?, need_str(a, 4, "yKey")?, need_str(a, 5, "zKey")?);
    let win = a.get(6).and_then(|v| v.as_f64()).filter(|w| *w > 0.0 && w.is_finite()).ok_or_else(|| crate::error::Error::Query("win must be a number > 0 (window length in time units)".into()))?;
    let o = parse_opts(opt_str(a, 7))?;
    let evs = select(view, label, o.filter.as_deref())?;
    for k in [idk, tk, xk, yk, zk] {
        require_key(view, &evs, k, "key")?;
    }
    // ---- observations per (track, window)
    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut obs: BTreeMap<(usize, i64), Vec<(f64, [f64; 3])>> = BTreeMap::new();
    let (mut used, mut skipped) = (0i64, 0i64);
    let mut names: Vec<String> = Vec::new();
    for b in &evs {
        let (Some(id), Some(t), Some(x), Some(y), Some(z)) = (group_of(view, b, idk), num(view, b, tk), num(view, b, xk), num(view, b, yk), num(view, b, zk)) else {
            skipped += 1;
            continue;
        };
        if !(t.is_finite() && x.is_finite() && y.is_finite() && z.is_finite()) {
            skipped += 1;
            continue;
        }
        let n = ids.len();
        let i = *ids.entry(id.clone()).or_insert_with(|| {
            names.push(id);
            n
        });
        obs.entry((i, (t / win).floor() as i64)).or_default().push((t, [x, y, z]));
        used += 1;
    }
    // ids sorted by name so that every ordering below is deterministic
    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_by(|&p, &q| names[p].cmp(&names[q]));
    let mut rank = vec![0usize; names.len()];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r;
    }
    let sorted_names: Vec<String> = order.iter().map(|&i| names[i].clone()).collect();
    // ---- per (track, window): mean position, least-squares velocity
    let mut windows: BTreeMap<i64, Vec<Tw>> = BTreeMap::new();
    let mut no_velocity = 0i64;
    let mut low_motion = 0i64; // track-windows whose speed is below 3 sigma of the velocity estimate: velocity linking has no power there
    for ((i, w), mut s) in obs {
        s.sort_by(|p, q| p.0.partial_cmp(&q.0).unwrap_or(std::cmp::Ordering::Equal));
        let n = s.len() as f64;
        let tbar = s.iter().map(|x| x.0).sum::<f64>() / n;
        let sxx: f64 = s.iter().map(|x| (x.0 - tbar) * (x.0 - tbar)).sum();
        if s.len() < o.min_samples || sxx <= 0.0 {
            no_velocity += 1;
            continue;
        }
        let mut p = [0.0; 3];
        let mut v = [0.0; 3];
        for k in 0..o.dim {
            let m = s.iter().map(|x| x.1[k]).sum::<f64>() / n;
            p[k] = m;
            v[k] = s.iter().map(|x| (x.0 - tbar) * (x.1[k] - m)).sum::<f64>() / sxx;
        }
        if s.len() > 2 {
            let rss: f64 = s.iter().map(|x| (0..o.dim).map(|k| { let r = x.1[k] - p[k] - (x.0 - tbar) * v[k]; r * r }).sum::<f64>()).sum();
            let sig_v = (rss / ((s.len() - 2) as f64 * o.dim as f64)).sqrt() / sxx.sqrt();
            if norm(v) < 3.0 * sig_v * (o.dim as f64).sqrt() {
                low_motion += 1;
            }
        }
        windows.entry(w).or_default().push(Tw { id: rank[i], p, v, speed: norm(v) });
    }
    for tw in windows.values_mut() {
        tw.sort_by_key(|t| t.id);
        if tw.len() > o.max_tracks {
            return bad(format!("{} tracks in one window exceeds max_tracks={}; narrow the filter or raise max_tracks (cost is quadratic)", tw.len(), o.max_tracks));
        }
    }
    // ---- pair links
    let mut co: BTreeMap<(usize, usize), (u32, u32)> = BTreeMap::new();
    for tw in windows.values() {
        for (x, ti) in tw.iter().enumerate() {
            for tj in &tw[x + 1..] {
                let e = co.entry((ti.id, tj.id)).or_insert((0, 0));
                e.0 += 1;
                let linked = ti.speed > 0.0 && tj.speed > 0.0 && dist(ti.p, tj.p) <= o.eps && {
                    let c = (ti.v[0] * tj.v[0] + ti.v[1] * tj.v[1] + ti.v[2] * tj.v[2]) / (ti.speed * tj.speed);
                    c >= o.cos_min && ti.speed.min(tj.speed) / ti.speed.max(tj.speed) >= o.speed_tol
                };
                if linked {
                    e.1 += 1;
                }
            }
        }
    }
    let n = sorted_names.len();
    let mut uf: Vec<usize> = (0..n).collect();
    for (&(i, j), &(c, l)) in &co {
        if c as usize >= o.min_windows && l as f64 >= o.persist * c as f64 {
            let (ri, rj) = (find(&mut uf, i), find(&mut uf, j));
            if ri != rj {
                uf[ri.max(rj)] = ri.min(rj);
            }
        }
    }
    let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let r = find(&mut uf, i);
        comps.entry(r).or_default().push(i);
    }
    let mut groups: Vec<Vec<usize>> = comps.into_values().filter(|m| m.len() >= o.min_size).collect();
    groups.sort_by(|p, q| q.len().cmp(&p.len()).then(p[0].cmp(&q[0])));
    // ---- per-group statistics
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (gi, m) in groups.iter().enumerate() {
        let inm = |id: usize| m.binary_search(&id).is_ok();
        let (mut wcount, mut coh_sum, mut dist_sum) = (0usize, 0.0, 0.0);
        let (mut first, mut last) = (i64::MAX, i64::MIN);
        let mut speeds: Vec<f64> = Vec::new();
        let mut used_windows: Vec<(i64, usize)> = Vec::new();
        for (&w, tw) in &windows {
            let s: Vec<&Tw> = tw.iter().filter(|t| inm(t.id)).collect();
            if s.len() < 2 {
                continue;
            }
            wcount += 1;
            first = first.min(w);
            last = last.max(w);
            let mut u = [0.0; 3];
            for t in &s {
                if t.speed > 0.0 {
                    for k in 0..3 {
                        u[k] += t.v[k] / t.speed;
                    }
                }
                speeds.push(t.speed);
            }
            coh_sum += norm(u) / s.len() as f64;
            let (mut d, mut np) = (0.0, 0usize);
            for (x, ti) in s.iter().enumerate() {
                for tj in &s[x + 1..] {
                    d += dist(ti.p, tj.p);
                    np += 1;
                }
            }
            dist_sum += d / np as f64;
            used_windows.push((w, s.len()));
        }
        if wcount == 0 {
            continue;
        }
        let coh = coh_sum / wcount as f64;
        let sm = speeds.iter().sum::<f64>() / speeds.len() as f64;
        let sd = (speeds.iter().map(|x| (x - sm) * (x - sm)).sum::<f64>() / speeds.len() as f64).sqrt();
        let (mut pf, mut pn) = (0.0, 0usize);
        for (x, &i) in m.iter().enumerate() {
            for &j in &m[x + 1..] {
                if let Some(&(c, l)) = co.get(&(i, j)) {
                    pf += l as f64 / c as f64;
                    pn += 1;
                }
            }
        }
        let (p_iso, p_bg) = if o.perm == 0 {
            (None, None)
        } else {
            let stream = o.seed.wrapping_add(gi as u64);
            let mut rng = Rng(stream);
            let mut rng_iso = Rng(stream ^ 0xD1B5_4A32_D192_ED03);
            let (mut ge, mut ge_iso) = (0usize, 0usize);
            for _ in 0..o.perm {
                let (mut acc, mut acc_iso) = (0.0, 0.0);
                for &(w, k) in &used_windows {
                    let pool = &windows[&w];
                    let mut idx: Vec<usize> = (0..pool.len()).collect();
                    let (mut u, mut ui) = ([0.0; 3], [0.0; 3]);
                    for i in 0..k {
                        let r = i + (rng.next() % (pool.len() - i) as u64) as usize;
                        idx.swap(i, r);
                        let t = &pool[idx[i]];
                        if t.speed > 0.0 {
                            for c in 0..3 {
                                u[c] += t.v[c] / t.speed;
                            }
                        }
                        let d = rng_iso.direction(o.dim);
                        for c in 0..3 {
                            ui[c] += d[c];
                        }
                    }
                    acc += norm(u) / k as f64;
                    acc_iso += norm(ui) / k as f64;
                }
                if acc / used_windows.len() as f64 >= coh - 1e-12 {
                    ge += 1;
                }
                if acc_iso / used_windows.len() as f64 >= coh - 1e-12 {
                    ge_iso += 1;
                }
            }
            (Some((1 + ge_iso) as f64 / (1 + o.perm) as f64), Some((1 + ge) as f64 / (1 + o.perm) as f64))
        };
        let verdict = match (p_iso, p_bg) {
            (Some(pi), Some(pb)) if pi <= o.p_max && coh >= o.coh_min => {
                if pb <= o.p_max {
                    "coordinated"
                } else {
                    "coordinated (whole scene)"
                }
            }
            (None, _) if coh >= o.coh_min => "coherent (untested)",
            _ => "co-located",
        };
        let mut names_out = m.iter().take(12).map(|&i| sorted_names[i].as_str()).collect::<Vec<_>>().join(",");
        if m.len() > 12 {
            names_out.push_str(",...");
        }
        rows.push(vec![
            Value::Str(format!("G{}", gi + 1)),
            Value::Int(m.len() as i64),
            Value::Str(names_out),
            Value::Int(wcount as i64),
            f(first as f64 * win),
            f(last as f64 * win),
            f(if pn > 0 { pf / pn as f64 } else { 0.0 }),
            f(dist_sum / wcount as f64),
            f(coh),
            f(sm),
            f(if sm > 0.0 { sd / sm } else { 0.0 }),
            fo(p_iso),
            fo(p_bg),
            Value::Str(verdict.into()),
        ]);
    }
    let summary = vec![
        Value::Str("(summary)".into()),
        Value::Int(n as i64),
        Value::Str(format!("tracks={n}; windows={}; observations_used={used}; observations_skipped={skipped}; track_windows_without_velocity={no_velocity}; low_motion_track_windows={low_motion} of {}; groups={}", windows.len(), windows.values().map(|w| w.len()).sum::<usize>(), rows.len())),
        Value::Int(windows.len() as i64),
        Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null,
        Value::Str(String::new()),
    ];
    rows.insert(0, summary);
    Ok((vec!["group", "size", "members", "windows", "first_window", "last_window", "persistence", "mean_pair_dist", "heading_coherence", "speed_mean", "speed_cv", "p_coherence", "p_background", "verdict"], rows))
}

/// `swarm.scan(label, idKey, timeKey, xKey, yKey, zKey, wins[, opts])`: `swarm.detect` at several window lengths
/// (`wins` is a comma list such as `'2,5,10,20'`), because the right window is set by sensor noise against drift speed and
/// a single window can miss a slow or noisy swarm entirely. Output: `win, group, size, members, windows, persistence,
/// mean_pair_dist, heading_coherence, p_coherence, p_background, verdict`, one row per group per window, plus a
/// `(summary)` row per window. Groups reported at several windows with the same members are the same swarm seen at several scales.
fn scan(view: &View, a: &[Value]) -> Result<Rows> {
    let list = a.get(6).and_then(|v| if let Value::Str(s) = v { Some(s.clone()) } else { None }).ok_or_else(|| crate::error::Error::Query("wins must be a string such as '2,5,10'".into()))?;
    let wins: Vec<f64> = list.split(',').map(|x| x.trim().parse::<f64>().ok().filter(|w| *w > 0.0 && w.is_finite())).collect::<Option<Vec<_>>>().ok_or_else(|| crate::error::Error::Query("wins must be positive numbers separated by commas".into()))?;
    if wins.is_empty() || wins.len() > 12 {
        return bad("wins must hold 1 to 12 window lengths");
    }
    let mut out: Vec<Vec<Value>> = Vec::new();
    let mut cols: Vec<&'static str> = Vec::new();
    for w in wins {
        let mut args: Vec<Value> = a.to_vec();
        args[6] = Value::Float(w);
        let (c, rows) = detect(view, &args)?;
        let keep = ["group", "size", "members", "windows", "persistence", "mean_pair_dist", "heading_coherence", "p_coherence", "p_background", "verdict"];
        let pos: Vec<usize> = keep.iter().map(|k| c.iter().position(|x| x == k).unwrap()).collect();
        cols = std::iter::once("win").chain(keep.iter().copied()).collect();
        for r in rows {
            let mut v = vec![Value::Float(w)];
            v.extend(pos.iter().map(|&i| r[i].clone()));
            out.push(v);
        }
    }
    Ok((cols, out))
}
