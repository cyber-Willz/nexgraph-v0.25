//! Deterministic k-means (k-means++ seeding, 10 restarts) for spectral embeddings.

pub(crate) fn kmeans(points: &[Vec<f64>], k: usize, seed: u64) -> Vec<usize> {
    let n = points.len();
    let dim = points[0].len();
    let mut s = seed | 1;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    };
    let d2 = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f64>();
    let mut best = (f64::INFINITY, vec![0usize; n]);
    for _ in 0..10 {
        // k-means++ seeding
        let mut cent: Vec<Vec<f64>> = vec![points[(rnd() * n as f64) as usize % n].clone()];
        while cent.len() < k {
            let dist: Vec<f64> = points.iter().map(|p| cent.iter().map(|c| d2(p, c)).fold(f64::INFINITY, f64::min)).collect();
            let total: f64 = dist.iter().sum();
            let mut pick = rnd() * total;
            let mut chosen = n - 1;
            for (i, d) in dist.iter().enumerate() {
                if pick <= *d {
                    chosen = i;
                    break;
                }
                pick -= d;
            }
            cent.push(points[chosen].clone());
        }
        let mut asg = vec![0usize; n];
        for _ in 0..100 {
            let mut changed = false;
            for (i, p) in points.iter().enumerate() {
                let c = (0..k).min_by(|&a, &b| d2(p, &cent[a]).total_cmp(&d2(p, &cent[b]))).unwrap();
                if asg[i] != c {
                    asg[i] = c;
                    changed = true;
                }
            }
            let mut sum = vec![vec![0.0; dim]; k];
            let mut cnt = vec![0usize; k];
            for (i, p) in points.iter().enumerate() {
                cnt[asg[i]] += 1;
                for d in 0..dim {
                    sum[asg[i]][d] += p[d];
                }
            }
            for c in 0..k {
                if cnt[c] > 0 {
                    cent[c] = sum[c].iter().map(|x| x / cnt[c] as f64).collect();
                }
            }
            if !changed {
                break;
            }
        }
        let inertia: f64 = points.iter().enumerate().map(|(i, p)| d2(p, &cent[asg[i]])).sum();
        if inertia < best.0 {
            best = (inertia, asg);
        }
    }
    best.1
}

