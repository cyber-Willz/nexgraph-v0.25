//! Dynamic values, property maps and the binary codec used by the WAL and
//! checkpoints (varints, CRC-32).

use crate::error::{Error, Result};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

/// Identifier shared by nodes and edges (one id space; ids are never reused
/// within a database lifetime except after a rolled-back transaction).
pub type Id = u64;
pub type Props = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    /// Dense f32 embedding (usable with vector indexes).
    Vector(Vec<f32>),
    /// Reference to a node (query results / expressions only; not storable).
    Node(Id),
    /// Reference to an edge or hyperedge (query results only; not storable).
    Edge(Id),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::List(_) => "list",
            Value::Vector(_) => "vector",
            Value::Node(_) => "node",
            Value::Edge(_) => "edge",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Interpret as an embedding: `Vector`, or a list of numbers.
    pub fn to_vector(&self) -> Option<Vec<f32>> {
        match self {
            Value::Vector(v) => Some(v.clone()),
            Value::List(l) => l.iter().map(|x| x.as_f64().map(|f| f as f32)).collect(),
            _ => None,
        }
    }

    /// Three-valued ordering used by comparison operators. `None` when the
    /// types are not comparable (or NaN is involved).
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        use Value::*;
        match (self, other) {
            (Int(a), Int(b)) => Some(a.cmp(b)),
            (Int(_) | Float(_), Int(_) | Float(_)) => self.as_f64()?.partial_cmp(&other.as_f64()?),
            (Str(a), Str(b)) => Some(a.cmp(b)),
            (Bool(a), Bool(b)) => Some(a.cmp(b)),
            (Node(a), Node(b)) | (Edge(a), Edge(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// Equality with numeric coercion (1 = 1.0).
    pub fn loose_eq(&self, other: &Value) -> bool {
        use Value::*;
        match (self, other) {
            (List(a), List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.loose_eq(y)),
            (Vector(a), Vector(b)) => a == b,
            (Null, Null) => true,
            _ => self.compare(other) == Some(Ordering::Equal),
        }
    }

    /// Total order for ORDER BY (nulls last, then by type rank, then value).
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Bool(_) => 0,
                Value::Int(_) | Value::Float(_) => 1,
                Value::Str(_) => 2,
                Value::List(_) | Value::Vector(_) => 3,
                Value::Node(_) => 4,
                Value::Edge(_) => 5,
                Value::Null => 6,
            }
        }
        match self.compare(other) {
            Some(o) => o,
            None => match (self, other) {
                (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
                _ => rank(self).cmp(&rank(other)),
            },
        }
    }

    pub(crate) fn check_storable(&self) -> Result<()> {
        match self {
            Value::Node(_) | Value::Edge(_) => Err(Error::Invalid(
                "node/edge references cannot be stored as property values".into(),
            )),
            Value::List(l) => l.iter().try_for_each(|v| v.check_storable()),
            Value::Float(f) if f.is_nan() => Err(Error::Invalid("NaN cannot be stored".into())),
            _ => Ok(()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Str(s) => write!(f, "'{s}'"),
            Value::List(l) => {
                write!(f, "[")?;
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Vector(v) => {
                write!(f, "vec[")?;
                for (i, x) in v.iter().take(6).enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{x:.3}")?;
                }
                if v.len() > 6 {
                    write!(f, ", … ({} dims)", v.len())?;
                }
                write!(f, "]")
            }
            Value::Node(id) => write!(f, "(#{id})"),
            Value::Edge(id) => write!(f, "[#{id}]"),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}
impl From<Vec<f32>> for Value {
    fn from(v: Vec<f32>) -> Self {
        Value::Vector(v)
    }
}

/// Build a property map from `(key, value)` pairs.
pub fn props<K: Into<String>, V: Into<Value>>(items: impl IntoIterator<Item = (K, V)>) -> Props {
    items.into_iter().map(|(k, v)| (k.into(), v.into())).collect()
}

// ---------------------------------------------------------------- codec

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (IEEE 802.3).
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

#[derive(Default)]
pub struct Enc {
    pub buf: Vec<u8>,
}

impl Enc {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    pub fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    pub fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn str(&mut self, s: &str) {
        self.varint(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
    pub fn ids(&mut self, ids: &[Id]) {
        self.varint(ids.len() as u64);
        for &i in ids {
            self.varint(i);
        }
    }
    pub fn strs(&mut self, s: &[String]) {
        self.varint(s.len() as u64);
        for x in s {
            self.str(x);
        }
    }
    pub fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.u8(0),
            Value::Bool(false) => self.u8(1),
            Value::Bool(true) => self.u8(2),
            Value::Int(i) => {
                self.u8(3);
                self.zigzag(*i)
            }
            Value::Float(x) => {
                self.u8(4);
                self.f64(*x)
            }
            Value::Str(s) => {
                self.u8(5);
                self.str(s)
            }
            Value::List(l) => {
                self.u8(6);
                self.varint(l.len() as u64);
                for x in l {
                    self.value(x);
                }
            }
            Value::Vector(v) => {
                self.u8(7);
                self.varint(v.len() as u64);
                for x in v {
                    self.buf.extend_from_slice(&x.to_le_bytes());
                }
            }
            Value::Node(i) => {
                self.u8(8);
                self.varint(*i)
            }
            Value::Edge(i) => {
                self.u8(9);
                self.varint(*i)
            }
        }
    }
    pub fn props(&mut self, p: &Props) {
        self.varint(p.len() as u64);
        for (k, v) in p {
            self.str(k);
            self.value(v);
        }
    }
}

pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn corrupt<T>(m: &str) -> Result<T> {
    Err(Error::Corrupt(m.to_string()))
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Dec { buf, pos: 0 }
    }
    pub fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
    /// Undecoded remainder of the buffer.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            return corrupt("unexpected end of record");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift >= 64 {
                return corrupt("varint overflow");
            }
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }
    pub fn zigzag(&mut self) -> Result<i64> {
        let u = self.varint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    pub fn f64(&mut self) -> Result<f64> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(f64::from_le_bytes(a))
    }
    fn len(&mut self) -> Result<usize> {
        let n = self.varint()? as usize;
        if n > self.buf.len() - self.pos + 1 && n > (1 << 28) {
            return corrupt("implausible length");
        }
        Ok(n)
    }
    pub fn str(&mut self) -> Result<String> {
        let n = self.len()?;
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| Error::Corrupt("invalid utf-8".into()))
    }
    pub fn ids(&mut self) -> Result<Vec<Id>> {
        let n = self.len()?;
        (0..n).map(|_| self.varint()).collect()
    }
    pub fn strs(&mut self) -> Result<Vec<String>> {
        let n = self.len()?;
        (0..n).map(|_| self.str()).collect()
    }
    pub fn value(&mut self) -> Result<Value> {
        self.value_depth(0)
    }
    fn value_depth(&mut self, depth: u32) -> Result<Value> {
        if depth > 64 {
            return corrupt("value nesting too deep");
        }
        Ok(match self.u8()? {
            0 => Value::Null,
            1 => Value::Bool(false),
            2 => Value::Bool(true),
            3 => Value::Int(self.zigzag()?),
            4 => Value::Float(self.f64()?),
            5 => Value::Str(self.str()?),
            6 => {
                let n = self.len()?;
                let mut l = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    l.push(self.value_depth(depth + 1)?);
                }
                Value::List(l)
            }
            7 => {
                let n = self.len()?;
                let raw = self.take(n.checked_mul(4).ok_or_else(|| Error::Corrupt("len".into()))?)?;
                Value::Vector(
                    raw.chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect(),
                )
            }
            8 => Value::Node(self.varint()?),
            9 => Value::Edge(self.varint()?),
            t => return Err(Error::Corrupt(format!("unknown value tag {t}"))),
        })
    }
    pub fn props(&mut self) -> Result<Props> {
        let n = self.len()?;
        let mut p = Props::new();
        for _ in 0..n {
            let k = self.str()?;
            let v = self.value()?;
            p.insert(k, v);
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn value_roundtrip() {
        let v = Value::List(vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(-123456789012),
            Value::Float(3.5),
            Value::Str("héllo".into()),
            Value::Vector(vec![1.0, -2.5, 0.0]),
        ]);
        let mut e = Enc::new();
        e.value(&v);
        let mut d = Dec::new(&e.buf);
        assert_eq!(d.value().unwrap(), v);
        assert!(d.done());
    }

    #[test]
    fn numeric_coercion() {
        assert!(Value::Int(1).loose_eq(&Value::Float(1.0)));
        assert_eq!(Value::Int(2).compare(&Value::Float(1.5)), Some(Ordering::Greater));
        assert_eq!(Value::Str("a".into()).compare(&Value::Int(1)), None);
    }

    #[test]
    fn truncated_input_is_error() {
        let mut e = Enc::new();
        e.value(&Value::Str("abcdef".into()));
        let cut = &e.buf[..e.buf.len() - 2];
        assert!(Dec::new(cut).value().is_err());
    }
}
