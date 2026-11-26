//! Entity bodies and the logical operations that make up a commit.

use crate::error::{Error, Result};
use crate::value::{Dec, Enc, Id, Props};

/// Distance metric for vector indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    L2,
}

/// One immutable version of a node or edge.
///
/// Edges are *directed hyperedges*: a `tail` set and a `head` set. A classic
/// binary edge is `tail = [a], head = [b]`. An undirected hyperedge keeps all
/// members in `tail` and leaves `head` empty.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Node { labels: Vec<String>, props: Props },
    Edge { label: String, tail: Vec<Id>, head: Vec<Id>, weight: f64, props: Props },
}

impl Body {
    pub fn props(&self) -> &Props {
        match self {
            Body::Node { props, .. } | Body::Edge { props, .. } => props,
        }
    }
    pub fn is_edge(&self) -> bool {
        matches!(self, Body::Edge { .. })
    }
    pub fn has_label(&self, l: &str) -> bool {
        match self {
            Body::Node { labels, .. } => labels.iter().any(|x| x == l),
            Body::Edge { label, .. } => label == l,
        }
    }
    pub fn label_list(&self) -> Vec<&str> {
        match self {
            Body::Node { labels, .. } => labels.iter().map(|s| s.as_str()).collect(),
            Body::Edge { label, .. } => vec![label.as_str()],
        }
    }
    /// All distinct members of an edge (tail then head), empty for nodes.
    pub fn members(&self) -> Vec<Id> {
        match self {
            Body::Edge { tail, head, .. } => {
                let mut m: Vec<Id> = Vec::with_capacity(tail.len() + head.len());
                for &x in tail.iter().chain(head.iter()) {
                    if !m.contains(&x) {
                        m.push(x);
                    }
                }
                m
            }
            _ => Vec::new(),
        }
    }
}

/// A logical, deterministic state change. Replaying the same ops against the
/// same state always yields the same result.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    CreateNode { id: Id, labels: Vec<String>, props: Props },
    CreateEdge { id: Id, label: String, tail: Vec<Id>, head: Vec<Id>, weight: f64, props: Props },
    Update {
        id: Id,
        set: Props,
        remove: Vec<String>,
        add_labels: Vec<String>,
        remove_labels: Vec<String>,
        weight: Option<f64>,
    },
    /// Deleting a node also deletes every incident edge/hyperedge.
    Delete { id: Id },
    CreatePropIndex { label: String, key: String },
    CreateVectorIndex { label: String, key: String, dim: u32, metric: Metric },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub ts: u64,
    pub ops: Vec<Op>,
}

impl Commit {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.varint(self.ts);
        e.varint(self.ops.len() as u64);
        for op in &self.ops {
            encode_op(&mut e, op);
        }
        e.buf
    }

    /// `varint(prefix)` followed by [`Commit::encode`], built in one buffer (a checkpoint writes `next_id` first;
    /// concatenating two buffers copied the whole snapshot an extra time).
    pub fn encode_with_prefix(&self, prefix: u64) -> Vec<u8> {
        let mut e = Enc::new();
        e.varint(prefix);
        e.varint(self.ts);
        e.varint(self.ops.len() as u64);
        for op in &self.ops {
            encode_op(&mut e, op);
        }
        e.buf
    }

    pub fn decode(buf: &[u8]) -> Result<Commit> {
        let mut d = Dec::new(buf);
        let ts = d.varint()?;
        let n = d.varint()? as usize;
        let mut ops = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            ops.push(decode_op(&mut d)?);
        }
        if !d.done() {
            return Err(Error::Corrupt("trailing bytes in commit".into()));
        }
        Ok(Commit { ts, ops })
    }
}

/// Encode a checkpoint payload (`varint(prefix)`, then a commit of `count` ops) from an iterator, one op at a time,
/// without ever materialising the op list. Byte-identical to `Commit { ts, ops }.encode_with_prefix(prefix)`.
pub fn encode_stream(prefix: u64, ts: u64, count: usize, ops: impl Iterator<Item = Op>) -> Vec<u8> {
    let mut e = Enc::new();
    e.varint(prefix);
    e.varint(ts);
    e.varint(count as u64);
    let mut n = 0usize;
    for op in ops {
        encode_op(&mut e, &op);
        n += 1;
    }
    debug_assert_eq!(n, count);
    e.buf
}

fn encode_op(e: &mut Enc, op: &Op) {
    match op {
        Op::CreateNode { id, labels, props } => {
            e.u8(1);
            e.varint(*id);
            e.strs(labels);
            e.props(props);
        }
        Op::CreateEdge { id, label, tail, head, weight, props } => {
            e.u8(2);
            e.varint(*id);
            e.str(label);
            e.ids(tail);
            e.ids(head);
            e.f64(*weight);
            e.props(props);
        }
        Op::Update { id, set, remove, add_labels, remove_labels, weight } => {
            e.u8(3);
            e.varint(*id);
            e.props(set);
            e.strs(remove);
            e.strs(add_labels);
            e.strs(remove_labels);
            match weight {
                Some(w) => {
                    e.u8(1);
                    e.f64(*w)
                }
                None => e.u8(0),
            }
        }
        Op::Delete { id } => {
            e.u8(4);
            e.varint(*id);
        }
        Op::CreatePropIndex { label, key } => {
            e.u8(5);
            e.str(label);
            e.str(key);
        }
        Op::CreateVectorIndex { label, key, dim, metric } => {
            e.u8(6);
            e.str(label);
            e.str(key);
            e.varint(*dim as u64);
            e.u8(match metric {
                Metric::Cosine => 0,
                Metric::L2 => 1,
            });
        }
    }
}

fn decode_op(d: &mut Dec) -> Result<Op> {
    Ok(match d.u8()? {
        1 => Op::CreateNode { id: d.varint()?, labels: d.strs()?, props: d.props()? },
        2 => Op::CreateEdge {
            id: d.varint()?,
            label: d.str()?,
            tail: d.ids()?,
            head: d.ids()?,
            weight: d.f64()?,
            props: d.props()?,
        },
        3 => {
            let id = d.varint()?;
            let set = d.props()?;
            let remove = d.strs()?;
            let add_labels = d.strs()?;
            let remove_labels = d.strs()?;
            let weight = if d.u8()? == 1 { Some(d.f64()?) } else { None };
            Op::Update { id, set, remove, add_labels, remove_labels, weight }
        }
        4 => Op::Delete { id: d.varint()? },
        5 => Op::CreatePropIndex { label: d.str()?, key: d.str()? },
        6 => Op::CreateVectorIndex {
            label: d.str()?,
            key: d.str()?,
            dim: d.varint()? as u32,
            metric: match d.u8()? {
                0 => Metric::Cosine,
                1 => Metric::L2,
                m => return Err(Error::Corrupt(format!("unknown metric {m}"))),
            },
        },
        t => return Err(Error::Corrupt(format!("unknown op tag {t}"))),
    })
}
