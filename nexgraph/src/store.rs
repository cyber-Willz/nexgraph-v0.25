//! MVCC in-memory store: version chains, adjacency, secondary indexes and the
//! read-only `View` used by queries and algorithms.

use crate::error::{Error, Result};
use crate::hnsw::Hnsw;
use crate::model::{Body, Metric, Op};
use crate::value::{Id, Props, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

pub const OPEN: u64 = u64::MAX;

/// Multiplicative hasher for dense integer ids (SipHash is wasted on them).
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);
impl std::hash::Hasher for IdHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    #[inline]
    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0 ^ x).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
type IdMap<V> = HashMap<Id, V, std::hash::BuildHasherDefault<IdHasher>>;
type IdSet = HashSet<Id, std::hash::BuildHasherDefault<IdHasher>>;

#[derive(Clone)]
pub struct Version {
    pub from: u64,
    pub to: u64,
    pub body: Arc<Body>,
}

/// Compact per-entity metadata of the *latest* version, kept in one dense
/// array indexed by id (the columnar idea from Polars/Kuzu: hot predicates and
/// endpoints are answered from a flat array of small structs, without chasing
/// entity -> version chain -> Arc<Body> -> member vectors). Labels are
/// dictionary-encoded to u32. Valid for a reader at `ts` only when
/// `from <= ts`; older readers fall back to the version chain.
#[derive(Clone, Copy)]
pub struct Cm {
    pub from: u64,
    pub to: u64,
    pub lab: u32,
    pub t0: Id,
    pub h0: Id,
    pub flags: u8,
}
const F_PRESENT: u8 = 1;
const F_EDGE: u8 = 2;
const F_BINARY: u8 = 4; // edge with exactly one tail and one head member
const F_MULTI: u8 = 8; // node whose label set is not exactly one label
pub const NO_LABEL: u32 = u32::MAX;
const CM_EMPTY: Cm = Cm { from: 0, to: 0, lab: NO_LABEL, t0: 0, h0: 0, flags: 0 };

pub enum EdgeFast {
    Gone,
    Binary { lab: u32, t: Id, h: Id },
    Complex,
}

/// A one-hop pattern reduced to plain data (so it can be shared across threads).
#[derive(Clone, Copy)]
pub struct HopSpec<'s> {
    pub dir: Direction,
    pub elab: Option<&'s str>,
    pub nlab: Option<&'s str>,
    pub elab_id: Option<u32>,
    pub nlab_id: Option<u32>,
}

pub struct Entity {
    pub versions: Vec<Version>,
    /// For nodes: every edge id that ever touched this node (filtered by
    /// visibility on read).
    pub incident: Vec<Id>,
    /// Directed edges in which this node is a tail member / a head member
    /// (halves the work of a directed hop, like Kuzu's forward/backward CSR).
    pub outs: Vec<Id>,
    pub ins: Vec<Id>,
}

impl Entity {
    fn at(&self, ts: u64) -> Option<&Version> {
        self.versions.iter().rev().find(|v| v.from <= ts && ts < v.to)
    }
    fn live(&self) -> Option<&Version> {
        self.versions.last().filter(|v| v.to == OPEN)
    }
}

/// Orderable key for the property index (numbers are widened to f64; the
/// index returns candidates and callers re-verify, so collisions are safe).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IKey {
    Bool(bool),
    Num(u64),
    Str(String),
}

fn ikey(v: &Value) -> Option<IKey> {
    match v {
        Value::Bool(b) => Some(IKey::Bool(*b)),
        Value::Int(_) | Value::Float(_) => {
            let f = v.as_f64()?;
            let f = if f == 0.0 { 0.0 } else { f }; // fold -0.0
            let b = f.to_bits();
            Some(IKey::Num(if b >> 63 == 1 { !b } else { b | (1 << 63) }))
        }
        Value::Str(s) => Some(IKey::Str(s.clone())),
        _ => None,
    }
}

/// Ids carrying a label. Ids normally arrive in increasing order (bulk loads), so they are appended
/// to a sorted run; only out-of-order or repeated ids go to the B-tree.
#[derive(Default)]
struct LabelSet {
    run: Vec<Id>,
    rest: BTreeSet<Id>,
}

impl LabelSet {
    fn insert(&mut self, id: Id) {
        match self.run.last() {
            Some(&l) if id <= l => {
                if self.run.binary_search(&id).is_err() {
                    self.rest.insert(id);
                }
            }
            _ => self.run.push(id),
        }
    }
    /// Upper bound on the number of ids.
    fn len(&self) -> usize {
        self.run.len() + self.rest.len()
    }
    /// All ids, ascending and distinct.
    fn ids(&self) -> std::borrow::Cow<'_, [Id]> {
        if self.rest.is_empty() {
            return std::borrow::Cow::Borrowed(&self.run);
        }
        let mut v = self.run.clone();
        v.extend(self.rest.iter().copied());
        v.sort_unstable();
        v.dedup();
        std::borrow::Cow::Owned(v)
    }
}

struct VecIndex {
    hnsw: Hnsw,
}

#[derive(Default)]
pub struct Inner {
    pub(crate) ents: Vec<Option<Entity>>,
    pub(crate) cm: Vec<Cm>,
    lab_ids: HashMap<String, u32>,
    label_idx: HashMap<String, LabelSet>,
    prop_idx: HashMap<(String, String), BTreeMap<IKey, BTreeSet<Id>>>,
    vec_idx: HashMap<(String, String), VecIndex>,
    pub(crate) index_defs: Vec<Op>,
    /// Bumped by every `apply`; invalidates the adjacency snapshot.
    epoch: u64,
    /// Commit timestamp of the newest write. A reader at `ts >= last_ts` sees exactly the live state.
    last_ts: u64,
    csr: std::sync::Mutex<Option<Arc<Csr>>>,
}

/// Dense, read-only adjacency snapshot of the live graph (built lazily, dropped on the next write).
/// Out/in edges are stored as flat arrays (offsets + target + edge label), nodes as a flat label array,
/// so a whole-graph traversal is a sequential scan instead of entity -> version -> Arc<Body> chasing.
/// The out-direction is built with the snapshot, the in-direction on first use.
pub struct Csr {
    epoch: u64,
    pub usable: bool,
    /// 0 = not a live node, 1 = live node (zero or one label)
    nflag: Vec<u8>,
    nlab: Vec<u32>,
    edges: Vec<(u32, u32, u32)>, // (tail, head, label) of every live edge
    out: Adj,
    inn: std::sync::OnceLock<Adj>,
}

#[derive(Default)]
struct Adj {
    off: Vec<u32>,
    dst: Vec<u32>,
    lab: Vec<u32>,
}

impl Adj {
    /// Counting-sort the edge list by `key(edge)` (offsets double as write cursors, then shift back).
    fn build(n: usize, edges: &[(u32, u32, u32)], inward: bool) -> Adj {
        let mut off = vec![0u32; n + 1];
        for &(t, h, _) in edges {
            off[(if inward { h } else { t }) as usize] += 1;
        }
        let mut sum = 0u32;
        for o in off.iter_mut() {
            let c = *o;
            *o = sum;
            sum += c;
        }
        let (mut dst, mut lab) = (vec![0u32; edges.len()], vec![0u32; edges.len()]);
        for &(t, h, l) in edges {
            let (k, v) = if inward { (h, t) } else { (t, h) };
            let p = off[k as usize] as usize;
            dst[p] = v;
            lab[p] = l;
            off[k as usize] += 1;
        }
        // off[i] now holds the end of node i's run = start of node i+1
        off.rotate_right(1);
        off[0] = 0;
        Adj { off, dst, lab }
    }

    #[inline]
    fn run(&self, a: usize) -> (usize, usize) {
        if a + 1 >= self.off.len() {
            (0, 0)
        } else {
            (self.off[a] as usize, self.off[a + 1] as usize)
        }
    }
}

impl Csr {
    fn adj(&self, dir: Direction) -> Option<&Adj> {
        match dir {
            Direction::Out => Some(&self.out),
            Direction::In => Some(self.inn.get_or_init(|| Adj::build(self.nflag.len(), &self.edges, true))),
            Direction::Any => None,
        }
    }

    #[inline]
    fn hops(&self, adj: &Adj, a: usize, sp: &HopSpec) -> u64 {
        let (lo, hi) = adj.run(a);
        let mut n = 0u64;
        match (sp.elab_id, sp.nlab_id) {
            (None, None) => {
                for &v in &adj.dst[lo..hi] {
                    n += self.nflag[v as usize] as u64;
                }
            }
            (e, nl) => {
                for k in lo..hi {
                    if e.map_or(false, |l| l != adj.lab[k]) {
                        continue;
                    }
                    let v = adj.dst[k] as usize;
                    if self.nflag[v] != 0 && nl.map_or(true, |l| self.nlab[v] == l) {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// Number of (edge, target) matches of one hop from `a` (same semantics as `View::hop_count`).
    #[inline]
    pub fn hop_count(&self, a: Id, sp: &HopSpec) -> u64 {
        match self.adj(sp.dir) {
            Some(adj) => self.hops(adj, a as usize, sp),
            None => 0,
        }
    }

    /// Sum of `hop_count` over every live node whose label is `src` (any live node if `None`).
    pub fn total(&self, src: Option<u32>, sp: &HopSpec) -> u64 {
        let Some(adj) = self.adj(sp.dir) else { return 0 };
        let mut n = 0u64;
        for a in 0..self.nflag.len() {
            if self.nflag[a] != 0 && src.map_or(true, |l| self.nlab[a] == l) {
                n += self.hops(adj, a, sp);
            }
        }
        n
    }
}

impl Inner {
    pub fn new() -> Self {
        Self::default()
    }

    fn ent(&self, id: Id) -> Option<&Entity> {
        self.ents.get(id as usize).and_then(|e| e.as_ref())
    }

    pub(crate) fn live_body(&self, id: Id) -> Option<&Arc<Body>> {
        self.ent(id).and_then(|e| e.live()).map(|v| &v.body)
    }

    pub(crate) fn live_incident(&self, id: Id) -> Vec<Id> {
        match self.ent(id) {
            Some(e) => e.incident.iter().copied().filter(|&x| self.live_body(x).is_some()).collect(),
            None => Vec::new(),
        }
    }

    pub(crate) fn max_id(&self) -> Id {
        self.ents.len().saturating_sub(1) as Id
    }

    // ----------------------------------------------------------- indexing

    #[inline]
    fn index_label(&mut self, id: Id, l: &str) {
        if let Some(s) = self.label_idx.get_mut(l) {
            s.insert(id);
        } else {
            let mut ls = LabelSet::default();
            ls.insert(id);
            self.label_idx.insert(l.to_string(), ls);
        }
    }

    fn index_version(&mut self, id: Id, from: u64, body: &Body) {
        match body {
            Body::Node { labels, .. } => {
                for l in labels {
                    self.index_label(id, l);
                }
            }
            Body::Edge { label, .. } => self.index_label(id, label),
        }
        for ((label, key), tree) in self.prop_idx.iter_mut() {
            if body.has_label(label) {
                if let Some(k) = body.props().get(key).and_then(ikey) {
                    tree.entry(k).or_default().insert(id);
                }
            }
        }
        for ((label, key), vi) in self.vec_idx.iter_mut() {
            if body.has_label(label) {
                if let Some(Value::Vector(v)) = body.props().get(key) {
                    if v.len() == vi.hnsw.dim() {
                        vi.hnsw.insert(id, from, v);
                    }
                }
            }
        }
    }

    fn all_versions(&self) -> Vec<(Id, u64, Arc<Body>)> {
        let mut out = Vec::new();
        for (i, e) in self.ents.iter().enumerate() {
            if let Some(e) = e {
                for v in &e.versions {
                    out.push((i as Id, v.from, v.body.clone()));
                }
            }
        }
        out
    }

    // -------------------------------------------------------------- apply

    /// Apply validated ops at commit timestamp `ts`.
    pub fn apply(&mut self, ts: u64, ops: &[Op]) -> Result<()> {
        self.epoch += 1;
        self.last_ts = self.last_ts.max(ts);
        for op in ops {
            self.apply_one(ts, op)?;
        }
        Ok(())
    }

    /// Like `apply`, but consumes the ops: labels, member lists and properties are moved into the
    /// stored bodies instead of being cloned (the bulk-load hot path).
    pub fn apply_owned(&mut self, ts: u64, ops: Vec<Op>) -> Result<()> {
        self.epoch += 1;
        self.last_ts = self.last_ts.max(ts);
        // size the per-id arrays once instead of regrowing (and copying) them while ids arrive
        let top = ops.iter().filter_map(|o| match o { Op::CreateNode { id, .. } | Op::CreateEdge { id, .. } => Some(*id as usize + 1), _ => None }).max().unwrap_or(0);
        if top > self.ents.len() && top < (1 << 32) {
            self.ents.reserve_exact(top - self.ents.len());
            self.cm.reserve_exact(top.saturating_sub(self.cm.len()));
        }
        for op in ops {
            match op {
                Op::CreateNode { id, labels, props } => {
                    let body = Arc::new(Body::Node { labels, props });
                    self.insert_entity(id, ts, body)?;
                }
                Op::CreateEdge { id, label, tail, head, weight, props } => {
                    for &m in tail.iter().chain(head.iter()) {
                        match self.ents.get_mut(m as usize).and_then(|e| e.as_mut()) {
                            Some(e) => {
                                if e.incident.last() != Some(&id) {
                                    e.incident.push(id);
                                }
                            }
                            None => return Err(Error::Corrupt(format!("edge {id} references missing node {m}"))),
                        }
                    }
                    if !head.is_empty() {
                        for &m in &tail {
                            if let Some(e) = self.ents.get_mut(m as usize).and_then(|e| e.as_mut()) {
                                if e.outs.last() != Some(&id) {
                                    e.outs.push(id);
                                }
                            }
                        }
                        for &m in &head {
                            if let Some(e) = self.ents.get_mut(m as usize).and_then(|e| e.as_mut()) {
                                if e.ins.last() != Some(&id) {
                                    e.ins.push(id);
                                }
                            }
                        }
                    }
                    let body = Arc::new(Body::Edge { label, tail, head, weight, props });
                    self.insert_entity(id, ts, body)?;
                }
                other => self.apply_one(ts, &other)?,
            }
        }
        Ok(())
    }

    fn apply_one(&mut self, ts: u64, op: &Op) -> Result<()> {
        match op {
                Op::CreateNode { id, labels, props } => {
                    let body = Arc::new(Body::Node { labels: labels.clone(), props: props.clone() });
                    self.insert_entity(*id, ts, body)?;
                }
                Op::CreateEdge { id, label, tail, head, weight, props } => {
                    let body = Arc::new(Body::Edge {
                        label: label.clone(),
                        tail: tail.clone(),
                        head: head.clone(),
                        weight: *weight,
                        props: props.clone(),
                    });
                    for m in body.members() {
                        match self.ents.get_mut(m as usize).and_then(|e| e.as_mut()) {
                            Some(e) => {
                                if e.incident.last() != Some(id) {
                                    e.incident.push(*id);
                                }
                            }
                            None => return Err(Error::Corrupt(format!("edge {id} references missing node {m}"))),
                        }
                    }
                    if !head.is_empty() {
                        for m in tail {
                            if let Some(e) = self.ents.get_mut(*m as usize).and_then(|e| e.as_mut()) {
                                if e.outs.last() != Some(id) {
                                    e.outs.push(*id);
                                }
                            }
                        }
                        for m in head {
                            if let Some(e) = self.ents.get_mut(*m as usize).and_then(|e| e.as_mut()) {
                                if e.ins.last() != Some(id) {
                                    e.ins.push(*id);
                                }
                            }
                        }
                    }
                    self.insert_entity(*id, ts, body)?;
                }
                Op::Update { id, set, remove, add_labels, remove_labels, weight } => {
                    let cur = self
                        .live_body(*id)
                        .cloned()
                        .ok_or_else(|| Error::Corrupt(format!("update of non-live entity {id}")))?;
                    let mut nb = (*cur).clone();
                    match &mut nb {
                        Body::Node { labels, props } => {
                            for k in remove {
                                props.remove(k);
                            }
                            for (k, v) in set {
                                props.insert(k.clone(), v.clone());
                            }
                            labels.retain(|l| !remove_labels.contains(l));
                            for l in add_labels {
                                if !labels.contains(l) {
                                    labels.push(l.clone());
                                }
                            }
                        }
                        Body::Edge { props, weight: w, .. } => {
                            for k in remove {
                                props.remove(k);
                            }
                            for (k, v) in set {
                                props.insert(k.clone(), v.clone());
                            }
                            if let Some(nw) = weight {
                                *w = *nw;
                            }
                        }
                    }
                    let nb = Arc::new(nb);
                    let ent = self.ents[*id as usize].as_mut().unwrap();
                    let last = ent.versions.last_mut().unwrap();
                    if last.from == ts {
                        last.body = nb.clone();
                    } else {
                        last.to = ts;
                        ent.versions.push(Version { from: ts, to: OPEN, body: nb.clone() });
                    }
                    let from = ent.versions.last().unwrap().from;
                    self.refresh_cm(*id);
                    self.index_version(*id, from, &nb);
                }
                Op::Delete { id } => self.delete_entity(*id, ts),
                Op::CreatePropIndex { label, key } => {
                    let k = (label.clone(), key.clone());
                    if !self.prop_idx.contains_key(&k) {
                        let mut tree: BTreeMap<IKey, BTreeSet<Id>> = BTreeMap::new();
                        for (id, _, b) in self.all_versions() {
                            if b.has_label(label) {
                                if let Some(ik) = b.props().get(key).and_then(ikey) {
                                    tree.entry(ik).or_default().insert(id);
                                }
                            }
                        }
                        self.prop_idx.insert(k, tree);
                        self.index_defs.push(op.clone());
                    }
                }
                Op::CreateVectorIndex { label, key, dim, metric } => {
                    let k = (label.clone(), key.clone());
                    if !self.vec_idx.contains_key(&k) {
                        let mut h = Hnsw::new(*dim as usize, *metric);
                        for (id, from, b) in self.all_versions() {
                            if b.has_label(label) {
                                if let Some(Value::Vector(v)) = b.props().get(key) {
                                    if v.len() == *dim as usize {
                                        h.insert(id, from, v);
                                    }
                                }
                            }
                        }
                        self.vec_idx.insert(k, VecIndex { hnsw: h });
                        self.index_defs.push(op.clone());
                    }
                }
                    }
        Ok(())
    }

    fn insert_entity(&mut self, id: Id, ts: u64, body: Arc<Body>) -> Result<()> {
        let idx = id as usize;
        if self.ents.len() <= idx {
            self.ents.resize_with(idx + 1, || None);
        }
        if self.ents[idx].is_some() {
            return Err(Error::Corrupt(format!("duplicate entity id {id}")));
        }
        self.ents[idx] = Some(Entity { versions: vec![Version { from: ts, to: OPEN, body: body.clone() }], incident: Vec::new(), outs: Vec::new(), ins: Vec::new() });
        self.refresh_cm(id);
        self.index_version(id, ts, &body);
        Ok(())
    }

    fn delete_entity(&mut self, id: Id, ts: u64) {
        let Some(body) = self.live_body(id).cloned() else { return };
        if !body.is_edge() {
            for e in self.live_incident(id) {
                self.close(e, ts);
            }
        }
        self.close(id, ts);
    }

    fn close(&mut self, id: Id, ts: u64) {
        if let Some(Some(e)) = self.ents.get_mut(id as usize) {
            if let Some(v) = e.versions.last_mut() {
                if v.to == OPEN {
                    v.to = ts;
                    if let Some(c) = self.cm.get_mut(id as usize) {
                        c.to = ts;
                    }
                }
            }
        }
    }

    fn build_csr(&self) -> Csr {
        let n = self.cm.len();
        let mut c = Csr { epoch: self.epoch, usable: false, nflag: vec![], nlab: vec![], edges: vec![], out: Adj::default(), inn: Default::default() };
        if n >= u32::MAX as usize {
            return c;
        }
        let mut nflag = vec![0u8; n];
        let mut nlab = vec![NO_LABEL; n];
        let mut edges: Vec<(u32, u32, u32)> = Vec::new();
        for (i, m) in self.cm.iter().enumerate() {
            if m.flags & F_PRESENT == 0 || m.to != OPEN {
                continue;
            }
            if m.flags & F_EDGE != 0 {
                if m.flags & F_BINARY == 0 || (m.t0 as usize) >= n || (m.h0 as usize) >= n {
                    return c; // hyperedge / undirected edge: leave it to the general path
                }
                edges.push((m.t0 as u32, m.h0 as u32, m.lab));
            } else {
                if m.flags & F_MULTI != 0 {
                    return c;
                }
                nflag[i] = 1;
                nlab[i] = m.lab;
            }
        }
        c.out = Adj::build(n, &edges, false);
        c.nflag = nflag;
        c.nlab = nlab;
        c.edges = edges;
        c.usable = true;
        c
    }

    fn intern(&mut self, l: &str) -> u32 {
        if let Some(&i) = self.lab_ids.get(l) {
            return i;
        }
        let i = self.lab_ids.len() as u32;
        self.lab_ids.insert(l.to_string(), i);
        i
    }

    /// Recompute the compact metadata of `id` from its latest version.
    fn refresh_cm(&mut self, id: Id) {
        let idx = id as usize;
        if self.cm.len() <= idx {
            self.cm.resize(idx + 1, CM_EMPTY);
        }
        let Some(Some(e)) = self.ents.get(idx) else { return };
        let v = e.versions.last().unwrap();
        let (from, to, body) = (v.from, v.to, v.body.clone());
        let mut c = Cm { from, to, lab: NO_LABEL, t0: 0, h0: 0, flags: F_PRESENT };
        match &*body {
            Body::Node { labels, .. } => {
                match labels.len() {
                    0 => {}
                    1 => c.lab = self.intern(&labels[0]),
                    _ => c.flags |= F_MULTI,
                }
            }
            Body::Edge { label, tail, head, .. } => {
                c.flags |= F_EDGE;
                c.lab = self.intern(label);
                if tail.len() == 1 && head.len() == 1 {
                    c.flags |= F_BINARY;
                    c.t0 = tail[0];
                    c.h0 = head[0];
                }
            }
        }
        self.cm[idx] = c;
    }

    // ----------------------------------------------------------- validate

    /// Check that `ops` can be applied. Deletes are idempotent within a
    /// transaction; everything else must reference live entities.
    pub fn validate(&self, ops: &[Op]) -> Result<()> {
        let has_delete = ops.iter().any(|o| matches!(o, Op::Delete { .. }));
        let mut created: IdMap<(bool, Vec<Id>)> = IdMap::with_capacity_and_hasher(ops.len(), Default::default()); // id -> (is_edge, members; only kept when the batch deletes)
        let mut deleted: IdSet = IdSet::default();
        let alive = |id: Id, created: &IdMap<(bool, Vec<Id>)>, deleted: &IdSet| -> bool {
            !deleted.contains(&id) && (created.contains_key(&id) || self.live_body(id).is_some())
        };
        for op in ops {
            match op {
                Op::CreateNode { id, .. } => {
                    if created.contains_key(id) || self.ent(*id).is_some() {
                        return Err(Error::Invalid(format!("id {id} already exists")));
                    }
                    created.insert(*id, (false, vec![]));
                }
                Op::CreateEdge { id, tail, head, weight, .. } => {
                    if created.contains_key(id) || self.ent(*id).is_some() {
                        return Err(Error::Invalid(format!("id {id} already exists")));
                    }
                    if !weight.is_finite() {
                        return Err(Error::Invalid("edge weight must be finite".into()));
                    }
                    let mut members = Vec::new();
                    for &m in tail.iter().chain(head.iter()) {
                        let is_node = match created.get(&m) {
                            Some((e, _)) => !*e,
                            None => self.live_body(m).map_or(false, |b| !b.is_edge()),
                        };
                        if !is_node || !alive(m, &created, &deleted) {
                            return Err(Error::NotFound(m));
                        }
                        if has_delete {
                            members.push(m);
                        }
                    }
                    created.insert(*id, (true, members));
                }
                Op::Update { id, .. } => {
                    if !alive(*id, &created, &deleted) {
                        return Err(Error::NotFound(*id));
                    }
                }
                Op::Delete { id } => {
                    if deleted.contains(id) {
                        continue;
                    }
                    if !alive(*id, &created, &deleted) {
                        return Err(Error::NotFound(*id));
                    }
                    deleted.insert(*id);
                    let is_edge = created.get(id).map(|c| c.0).or_else(|| self.live_body(*id).map(|b| b.is_edge())).unwrap_or(true);
                    if !is_edge {
                        for e in self.live_incident(*id) {
                            deleted.insert(e);
                        }
                        for (eid, (ise, mem)) in created.iter() {
                            if *ise && mem.contains(id) {
                                deleted.insert(*eid);
                            }
                        }
                    }
                }
                Op::CreatePropIndex { .. } => {}
                Op::CreateVectorIndex { dim, .. } => {
                    if *dim == 0 {
                        return Err(Error::Invalid("vector dimension must be > 0".into()));
                    }
                }
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------- snapshot

    /// Ops that recreate the state live at the latest timestamp.
    pub fn snapshot_ops(&self) -> Vec<Op> {
        let mut ops: Vec<Op> = self.index_defs.clone();
        for (i, e) in self.ents.iter().enumerate() {
            if let Some(v) = e.as_ref().and_then(|e| e.live()) {
                if let Body::Node { labels, props } = &*v.body {
                    ops.push(Op::CreateNode { id: i as Id, labels: labels.clone(), props: props.clone() });
                }
            }
        }
        for (i, e) in self.ents.iter().enumerate() {
            if let Some(v) = e.as_ref().and_then(|e| e.live()) {
                if let Body::Edge { label, tail, head, weight, props } = &*v.body {
                    ops.push(Op::CreateEdge {
                        id: i as Id,
                        label: label.clone(),
                        tail: tail.clone(),
                        head: head.clone(),
                        weight: *weight,
                        props: props.clone(),
                    });
                }
            }
        }
        ops
    }

    /// Number of ops [`Inner::snapshot_iter`] yields.
    pub fn snapshot_len(&self) -> usize {
        self.index_defs.len() + self.ents.iter().flatten().filter(|e| e.live().map_or(false, |v| matches!(&*v.body, Body::Node { .. } | Body::Edge { .. }))).count()
    }

    /// The ops of [`Inner::snapshot_ops`], produced lazily (same order) so a checkpoint can encode them one at a time.
    pub fn snapshot_iter(&self) -> impl Iterator<Item = Op> + '_ {
        let nodes = self.ents.iter().enumerate().filter_map(|(i, e)| match e.as_ref().and_then(|e| e.live()).map(|v| &*v.body) {
            Some(Body::Node { labels, props }) => Some(Op::CreateNode { id: i as Id, labels: labels.clone(), props: props.clone() }),
            _ => None,
        });
        let edges = self.ents.iter().enumerate().filter_map(|(i, e)| match e.as_ref().and_then(|e| e.live()).map(|v| &*v.body) {
            Some(Body::Edge { label, tail, head, weight, props }) => {
                Some(Op::CreateEdge { id: i as Id, label: label.clone(), tail: tail.clone(), head: head.clone(), weight: *weight, props: props.clone() })
            }
            _ => None,
        });
        self.index_defs.iter().cloned().chain(nodes).chain(edges)
    }

    pub fn stats(&self) -> StoreStats {
        let mut s = StoreStats::default();
        for e in self.ents.iter().flatten() {
            s.versions += e.versions.len();
            if let Some(v) = e.live() {
                if v.body.is_edge() {
                    s.live_edges += 1
                } else {
                    s.live_nodes += 1
                }
            }
        }
        s.prop_indexes = self.prop_idx.len();
        s.vector_indexes = self.vec_idx.len();
        s
    }
}

#[derive(Default, Debug, Clone)]
pub struct StoreStats {
    pub live_nodes: usize,
    pub live_edges: usize,
    pub versions: usize,
    pub prop_indexes: usize,
    pub vector_indexes: usize,
}

// ------------------------------------------------------------------ View

/// Direction of traversal relative to a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Any,
}

/// A consistent, read-only snapshot at timestamp `ts`.
pub struct View<'a> {
    pub(crate) inner: &'a Inner,
    pub ts: u64,
}

impl<'a> View<'a> {
    pub fn body(&self, id: Id) -> Option<&'a Body> {
        self.inner.ent(id)?.at(self.ts).map(|v| &*v.body)
    }
    pub fn node(&self, id: Id) -> Option<&'a Body> {
        self.body(id).filter(|b| !b.is_edge())
    }
    pub fn edge(&self, id: Id) -> Option<&'a Body> {
        self.body(id).filter(|b| b.is_edge())
    }

    /// Dense adjacency snapshot of the live graph, when this reader sees exactly the live state
    /// and the graph has only simple directed edges and single-label nodes.
    pub fn csr(&self) -> Option<Arc<Csr>> {
        if self.ts < self.inner.last_ts {
            return None;
        }
        let mut g = self.inner.csr.lock().ok()?;
        match &*g {
            Some(c) if c.epoch == self.inner.epoch => {}
            _ => *g = Some(Arc::new(self.inner.build_csr())),
        }
        g.as_ref().filter(|c| c.usable).cloned()
    }

    /// Dictionary id of a label, if any entity ever carried it.
    pub fn label_id(&self, l: &str) -> Option<u32> {
        self.inner.lab_ids.get(l).copied()
    }

    /// Edge ids adjacent to `node` that can be crossed in `dir` (unfiltered).
    pub fn adj_ids(&self, node: Id, dir: Direction) -> &'a [Id] {
        match self.inner.ent(node) {
            Some(e) => match dir {
                Direction::Out => &e.outs,
                Direction::In => &e.ins,
                Direction::Any => &e.incident,
            },
            None => &[],
        }
    }

    #[inline]
    fn cm_vis(&self, id: Id) -> Option<(&'a Cm, Option<bool>)> {
        let c = self.inner.cm.get(id as usize)?;
        if c.flags & F_PRESENT == 0 {
            return None;
        }
        Some((c, if c.from <= self.ts { Some(self.ts < c.to) } else { None }))
    }

    /// Edge metadata answered from the flat array when possible.
    #[inline]
    pub fn edge_fast(&self, id: Id) -> EdgeFast {
        match self.cm_vis(id) {
            None => EdgeFast::Gone,
            Some((c, vis)) => {
                if c.flags & F_EDGE == 0 {
                    return EdgeFast::Gone;
                }
                match vis {
                    Some(false) => EdgeFast::Gone,
                    Some(true) if c.flags & F_BINARY != 0 => EdgeFast::Binary { lab: c.lab, t: c.t0, h: c.h0 },
                    _ => EdgeFast::Complex,
                }
            }
        }
    }

    /// Is `id` a visible node (with label id `lab` if given)? `None` = the flat
    /// array cannot decide (old snapshot or multi-label node): use the body.
    #[inline]
    pub fn node_fast(&self, id: Id, lab: Option<u32>) -> Option<bool> {
        match self.cm_vis(id) {
            None => Some(false),
            Some((c, vis)) => {
                if c.flags & F_EDGE != 0 {
                    return Some(false);
                }
                match vis {
                    Some(false) => Some(false),
                    Some(true) => match lab {
                        None => Some(true),
                        Some(l) if c.flags & F_MULTI == 0 => Some(c.lab == l),
                        _ => None,
                    },
                    None => None,
                }
            }
        }
    }

    /// Build a [`HopSpec`]; `None` if a named label has never been used (no matches possible).
    pub fn hop_spec<'s>(&self, dir: Direction, elab: Option<&'s str>, nlab: Option<&'s str>) -> Option<HopSpec<'s>> {
        let elab_id = match elab {
            Some(l) => Some(self.label_id(l)?),
            None => None,
        };
        let nlab_id = match nlab {
            Some(l) => Some(self.label_id(l)?),
            None => None,
        };
        Some(HopSpec { dir, elab, nlab, elab_id, nlab_id })
    }

    fn target_ok(&self, v: Id, sp: &HopSpec) -> bool {
        match self.node_fast(v, sp.nlab_id) {
            Some(b) => b,
            None => self.node(v).map_or(false, |b| sp.nlab.map_or(true, |l| b.has_label(l))),
        }
    }

    /// Number of (edge, target) matches of one hop from `a`, with no property
    /// filters and no variable bindings (same semantics as a pattern step).
    pub fn hop_count(&self, a: Id, sp: &HopSpec) -> u64 {
        let mut n = 0u64;
        for &eid in self.adj_ids(a, sp.dir) {
            match self.edge_fast(eid) {
                EdgeFast::Gone => {}
                EdgeFast::Binary { lab, t, h } => {
                    if sp.elab_id.map_or(false, |l| l != lab) {
                        continue;
                    }
                    let v = if t == h {
                        if t == a {
                            t
                        } else {
                            continue;
                        }
                    } else if sp.dir != Direction::In && t == a {
                        h
                    } else if sp.dir != Direction::Out && h == a {
                        t
                    } else {
                        continue;
                    };
                    if self.target_ok(v, sp) {
                        n += 1;
                    }
                }
                EdgeFast::Complex => {
                    let Some(b) = self.edge(eid) else { continue };
                    if sp.elab.map_or(false, |l| !b.has_label(l)) {
                        continue;
                    }
                    let _ = self.cross_each::<()>(a, b, sp.dir, |v| {
                        if self.target_ok(v, sp) {
                            n += 1;
                        }
                        Ok(true)
                    });
                }
            }
        }
        n
    }

    /// One past the largest entity id ever allocated (for dense per-id arrays).
    pub fn id_bound(&self) -> usize {
        self.inner.ents.len()
    }

    pub fn node_ids(&self) -> impl Iterator<Item = Id> + '_ {
        (0..self.inner.ents.len() as Id).filter(move |&i| self.node(i).is_some())
    }
    pub fn edge_ids(&self) -> impl Iterator<Item = Id> + '_ {
        (0..self.inner.ents.len() as Id).filter(move |&i| self.edge(i).is_some())
    }

    pub fn nodes_with_label(&self, label: &str) -> Vec<Id> {
        match self.inner.label_idx.get(label) {
            Some(s) => s.ids().iter().copied().filter(|&i| self.node(i).map_or(false, |b| b.has_label(label))).collect(),
            None => Vec::new(),
        }
    }
    pub fn edges_with_label(&self, label: &str) -> Vec<Id> {
        match self.inner.label_idx.get(label) {
            Some(s) => s.ids().iter().copied().filter(|&i| self.edge(i).map_or(false, |b| b.has_label(label))).collect(),
            None => Vec::new(),
        }
    }
    /// Cheap upper bound on the number of nodes carrying `label`.
    pub fn label_estimate(&self, label: &str) -> usize {
        self.inner.label_idx.get(label).map_or(0, |s| s.len())
    }
    pub fn total_estimate(&self) -> usize {
        self.inner.ents.len()
    }

    pub fn has_prop_index(&self, label: &str, key: &str) -> bool {
        self.inner.prop_idx.contains_key(&(label.to_string(), key.to_string()))
    }

    /// Nodes with `label` whose property `key` equals `val`. Uses the
    /// property index when one exists, otherwise scans the label.
    pub fn lookup(&self, label: &str, key: &str, val: &Value) -> Vec<Id> {
        let verify = |i: &Id| {
            self.node(*i).map_or(false, |b| b.has_label(label) && b.props().get(key).map_or(false, |v| v.loose_eq(val)))
        };
        if let Some(tree) = self.inner.prop_idx.get(&(label.to_string(), key.to_string())) {
            return match ikey(val) {
                Some(k) => tree.get(&k).map(|s| s.iter().copied().filter(verify).collect()).unwrap_or_default(),
                None => Vec::new(),
            };
        }
        self.nodes_with_label(label).into_iter().filter(|i| verify(i)).collect()
    }

    /// Nodes with `label` whose property `key` lies between the bounds, answered from the ordered property index
    /// (`(value, inclusive)`; `None` = unbounded). Comparison follows the query language: numbers compare with
    /// numbers, strings with strings, booleans with booleans, and nothing else is comparable, so a bound of one type
    /// never matches values of another. Returns `None` (caller must scan) when there is no index on `(label, key)`,
    /// no bound is given, or a bound is of a type the index cannot key (null, list, vector, ...). Result is sorted by
    /// id, like a label scan.
    pub fn lookup_range(&self, label: &str, key: &str, lo: Option<(&Value, bool)>, hi: Option<(&Value, bool)>) -> Option<Vec<Id>> {
        use std::cmp::Ordering;
        use std::ops::Bound::{Included, Unbounded};
        fn class(k: &IKey) -> u8 {
            match k {
                IKey::Bool(_) => 0,
                IKey::Num(_) => 1,
                IKey::Str(_) => 2,
            }
        }
        let tree = self.inner.prop_idx.get(&(label.to_string(), key.to_string()))?;
        let lk = match lo {
            Some((v, _)) => Some(ikey(v)?),
            None => None,
        };
        let hk = match hi {
            Some((v, _)) => Some(ikey(v)?),
            None => None,
        };
        let cls = match (&lk, &hk) {
            (None, None) => return None,
            (Some(a), Some(b)) if class(a) != class(b) => return Some(Vec::new()),
            (Some(a), _) | (_, Some(a)) => class(a),
        };
        // Index bounds are always inclusive (numbers are widened to f64, so exactness is re-checked below).
        if let (Some(a), Some(b)) = (&lk, &hk) {
            if a > b {
                return Some(Vec::new());
            }
        }
        let start = Included(lk.unwrap_or(match cls {
            0 => IKey::Bool(false),
            1 => IKey::Num(0),
            _ => IKey::Str(String::new()),
        }));
        let end = match hk {
            Some(k) => Included(k),
            None => match cls {
                0 => Included(IKey::Bool(true)),
                1 => Included(IKey::Num(u64::MAX)),
                _ => Unbounded,
            },
        };
        let in_bounds = |pv: &Value| {
            let ok = |b: Option<(&Value, bool)>, upper: bool| match b {
                None => true,
                Some((bv, inc)) => match pv.compare(bv) {
                    Some(Ordering::Equal) => inc,
                    Some(o) => (o == Ordering::Less) == upper,
                    None => false,
                },
            };
            ok(lo, false) && ok(hi, true)
        };
        let mut out: Vec<Id> = Vec::new();
        for (_, ids) in tree.range((start, end)) {
            for &i in ids {
                if let Some(b) = self.node(i) {
                    if b.has_label(label) && b.props().get(key).map_or(false, |pv| in_bounds(pv)) {
                        out.push(i);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    /// Visible edges touching `node` (no allocation; `incident` lists are
    /// de-duplicated at insertion).
    pub fn incident_iter(&self, node: Id) -> impl Iterator<Item = (Id, &'a Body)> + '_ {
        self.inner.ent(node).into_iter().flat_map(|e| e.incident.iter()).filter_map(move |&x| self.edge(x).map(|b| (x, b)))
    }

    /// Visible edges that can be crossed from `node` in `dir`.
    pub fn incident_dir(&self, node: Id, dir: Direction) -> impl Iterator<Item = (Id, &'a Body)> + '_ {
        self.inner
            .ent(node)
            .into_iter()
            .flat_map(move |e| match dir {
                Direction::Out => e.outs.iter(),
                Direction::In => e.ins.iter(),
                Direction::Any => e.incident.iter(),
            })
            .filter_map(move |&x| self.edge(x).map(|b| (x, b)))
    }

    pub fn incident(&self, node: Id) -> Vec<(Id, &'a Body)> {
        self.incident_iter(node).collect()
    }

    /// Calls `f` for every node reachable from `u` across `body` in `dir`
    /// (same set as `cross`, allocation-free for the binary-edge case).
    /// Returns false if `f` returned false (early stop).
    pub fn cross_each<E>(&self, u: Id, body: &Body, dir: Direction, mut f: impl FnMut(Id) -> std::result::Result<bool, E>) -> std::result::Result<bool, E> {
        let Body::Edge { tail, head, .. } = body else { return Ok(true) };
        if tail.len() == 1 && head.len() == 1 {
            let (t, h) = (tail[0], head[0]);
            if t == h {
                return if t == u { f(h) } else { Ok(true) };
            }
            if dir != Direction::In && t == u {
                return f(h);
            }
            if dir != Direction::Out && h == u {
                return f(t);
            }
            return Ok(true);
        }
        for v in self.cross(u, body, dir) {
            if !f(v)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Nodes reachable from `u` across edge `e` in `dir`.
    ///
    /// Hyperedge semantics: a directed edge leads from every tail member to
    /// every head member; an undirected edge (empty head) connects all of its
    /// members to each other and is only crossed with `Direction::Any`.
    pub fn cross(&self, u: Id, body: &Body, dir: Direction) -> Vec<Id> {
        let Body::Edge { tail, head, .. } = body else { return Vec::new() };
        let mut out = Vec::new();
        if head.is_empty() {
            if dir == Direction::Any && tail.contains(&u) {
                out.extend(tail.iter().copied().filter(|&x| x != u));
            }
            return out;
        }
        if dir != Direction::In && tail.contains(&u) {
            out.extend(head.iter().copied());
        }
        if dir != Direction::Out && head.contains(&u) {
            out.extend(tail.iter().copied());
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Distinct neighbours of `u` in `dir`.
    pub fn neighbors(&self, u: Id, dir: Direction) -> Vec<Id> {
        let mut out: Vec<Id> = self.incident(u).iter().flat_map(|(_, b)| self.cross(u, b, dir)).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn has_vector_index(&self, label: &str, key: &str) -> bool {
        self.inner.vec_idx.contains_key(&(label.to_string(), key.to_string()))
    }

    /// k nearest nodes by the index metric (HNSW; `exact = true` brute-forces
    /// the visible nodes instead). Returns `(node, distance)` ascending.
    /// Approximate mode over-fetches `max(ef, 4k)` candidates and drops hits
    /// whose version is not visible at this snapshot, so heavily-updated
    /// vectors can reduce recall; raise `ef` or use `exact`.
    pub fn vector_search(&self, label: &str, key: &str, query: &[f32], k: usize, ef: usize, exact: bool) -> Result<Vec<(Id, f32)>> {
        let vi = self
            .inner
            .vec_idx
            .get(&(label.to_string(), key.to_string()))
            .ok_or_else(|| Error::Invalid(format!("no vector index on :{label}({key})")))?;
        if query.len() != vi.hnsw.dim() {
            return Err(Error::Invalid(format!("query has {} dims, index expects {}", query.len(), vi.hnsw.dim())));
        }
        let current = |id: Id| -> Option<&'a Vec<f32>> {
            let b = self.node(id)?;
            if !b.has_label(label) {
                return None;
            }
            match b.props().get(key) {
                Some(Value::Vector(v)) => Some(v),
                _ => None,
            }
        };
        let mut out: Vec<(Id, f32)> = Vec::new();
        if exact {
            for id in self.nodes_with_label(label) {
                if let Some(v) = current(id) {
                    if v.len() == query.len() {
                        out.push((id, vi.hnsw.raw_distance(query, v)));
                    }
                }
            }
        } else {
            let mut seen = HashSet::new();
            for h in vi.hnsw.search(query, k, ef.max(4 * k).max(k)) {
                if !seen.insert(h.id) {
                    continue;
                }
                if let Some(v) = current(h.id) {
                    out.push((h.id, vi.hnsw.raw_distance(query, v)));
                }
            }
        }
        out.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        out.truncate(k);
        Ok(out)
    }

    pub fn stats(&self) -> StoreStats {
        self.inner.stats()
    }
}

pub(crate) fn _unused(_: &Props, _: Metric) {}
