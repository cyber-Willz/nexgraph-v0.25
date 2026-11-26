//! Lexer and recursive-descent parser for NQL, a Cypher-style language.

use crate::error::{Error, Result};
use crate::model::Metric;
use crate::store::Direction;
use crate::value::Value;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Param(String),
    P(&'static str),
    Eof,
}

const SYMS: [&str; 27] = [
    "<-", "->", "<=", ">=", "<>", "!=", "..", "+=", "(", ")", "[", "]", "{", "}", ",", ":", ".", "-", "=", "<", ">", "+", "*", "/", "%", "^", "|",
];

fn lex(src: &str) -> Result<Vec<(Tok, usize)>> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() || c == ';' {
            i += 1;
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        if c.is_ascii_alphabetic() || c == '_' {
            while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push((Tok::Ident(src[start..i].to_string()), start));
        } else if c == '$' {
            i += 1;
            let s = i;
            while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push((Tok::Param(src[s..i].to_string()), start));
        } else if c.is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let mut float = false;
            if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                float = true;
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    float = true;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    i = j;
                }
            }
            let t = &src[start..i];
            let tok = if float {
                Tok::Float(t.parse().map_err(|_| Error::Parse { msg: "bad number".into(), pos: start })?)
            } else {
                Tok::Int(t.parse().map_err(|_| Error::Parse { msg: "integer out of range".into(), pos: start })?)
            };
            out.push((tok, start));
        } else if c == '\'' || c == '"' {
            i += 1;
            let mut s = String::new();
            loop {
                if i >= b.len() {
                    return Err(Error::Parse { msg: "unterminated string".into(), pos: start });
                }
                let ch = src[i..].chars().next().unwrap();
                i += ch.len_utf8();
                if ch == c {
                    break;
                }
                if ch == '\\' {
                    let n = src[i..].chars().next().ok_or(Error::Parse { msg: "bad escape".into(), pos: i })?;
                    i += n.len_utf8();
                    s.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        o => o,
                    });
                } else {
                    s.push(ch);
                }
            }
            out.push((Tok::Str(s), start));
        } else if c == '`' {
            // `quoted identifier` (v0.12): any characters, a doubled backtick is a literal one. Real column names
            // carry spaces and symbols ("Land Area (Acres)", "Profit ($)") and could not be named in a query before.
            i += 1;
            let mut s = String::new();
            loop {
                let Some(ch) = src[i..].chars().next() else {
                    return Err(Error::Parse { msg: "unterminated `quoted identifier`".into(), pos: start });
                };
                i += ch.len_utf8();
                if ch == '`' {
                    if src[i..].starts_with('`') {
                        i += 1;
                        s.push('`');
                        continue;
                    }
                    break;
                }
                s.push(ch);
            }
            if s.is_empty() {
                return Err(Error::Parse { msg: "empty `quoted identifier`".into(), pos: start });
            }
            out.push((Tok::Ident(s), start));
        } else {
            let rest = &src[i..];
            match SYMS.iter().find(|s| rest.starts_with(**s)) {
                Some(s) => {
                    i += s.len();
                    out.push((Tok::P(s), start));
                }
                None => return Err(Error::Parse { msg: format!("unexpected character '{c}'"), pos: i }),
            }
        }
    }
    out.push((Tok::Eof, src.len()));
    Ok(out)
}

// ------------------------------------------------------------------ AST

#[derive(Clone, Debug)]
pub enum Expr {
    Lit(Value),
    Param(String),
    Var(String),
    Prop(Box<Expr>, String),
    Call { name: String, args: Vec<Expr>, distinct: bool, star: bool },
    List(Vec<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    IsNull(Box<Expr>, bool),
    Bin(Bin, Box<Expr>, Box<Expr>),
    /// `CASE [operand] WHEN a THEN b ... [ELSE c] END`
    Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, els: Option<Box<Expr>> },
    /// `list[i]` (negative counts from the end; out of range is null)
    Index(Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bin {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    StartsWith,
    EndsWith,
    Contains,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

#[derive(Clone, Debug, Default)]
pub struct NodePat {
    pub var: Option<std::rc::Rc<str>>,
    pub label: Option<String>,
    pub props: Vec<(String, Expr)>,
    /// Range bounds `(key, op, bound)` (op is Lt/Le/Gt/Ge, read as `n.key op bound`) pushed down from a top-level
    /// WHERE conjunct by the executor. Never produced by the parser; they only narrow the candidate set, the WHERE
    /// clause still runs afterwards.
    pub ranges: Vec<(String, Bin, Expr)>,
}

#[derive(Clone, Debug)]
pub struct EdgePat {
    pub var: Option<std::rc::Rc<str>>,
    pub label: Option<String>,
    pub props: Vec<(String, Expr)>,
    pub dir: Direction,
    pub varlen: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub struct PathPat {
    pub nodes: Vec<NodePat>,
    pub edges: Vec<EdgePat>,
}

#[derive(Clone, Debug)]
pub enum Pattern {
    Path(PathPat),
    Hyper { var: String, label: Option<String>, props: Vec<(String, Expr)> },
}

#[derive(Clone, Debug)]
pub enum CreateItem {
    Path(PathPat),
    Hyper { var: Option<String>, label: String, props: Vec<(String, Expr)>, tail: Vec<Expr>, head: Vec<Expr> },
}

#[derive(Clone, Debug)]
pub struct ReturnClause {
    pub distinct: bool,
    pub items: Vec<(Expr, String)>,
    pub order: Vec<(Expr, bool)>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

#[derive(Clone, Debug)]
pub enum Clause {
    Match { patterns: Vec<Pattern>, wher: Option<Expr>, optional: bool },
    /// `WITH ...`: project into new bindings (everything not projected is dropped), then filter.
    With { rc: ReturnClause, wher: Option<Expr> },
    Unwind { expr: Expr, var: String },
    /// `MERGE pattern [ON CREATE SET ...] [ON MATCH SET ...]`
    Merge { path: PathPat, on_create: Vec<(String, String, Expr)>, on_match: Vec<(String, String, Expr)> },
    Create(Vec<CreateItem>),
    Set(Vec<(String, String, Expr)>),
    Delete(Vec<String>),
    Return(ReturnClause),
    Call { name: String, args: Vec<Expr>, yields: Vec<(String, String)> },
    CreatePropIndex { label: String, key: String },
    CreateVectorIndex { label: String, key: String, dim: u32, metric: Metric },
}

#[derive(Clone, Debug)]
pub struct Statement {
    pub as_of: Option<u64>,
    pub clauses: Vec<Clause>,
}

impl Statement {
    pub fn has_writes(&self) -> bool {
        self.clauses.iter().any(|c| {
            matches!(c, Clause::Create(_) | Clause::Merge { .. } | Clause::Set(_) | Clause::Delete(_) | Clause::CreatePropIndex { .. } | Clause::CreateVectorIndex { .. })
        })
    }
}

// ---------------------------------------------------------------- parser

struct P {
    toks: Vec<(Tok, usize)>,
    i: usize,
    /// Current expression nesting (parentheses, lists, call arguments).
    depth: usize,
    /// Expression nodes built so far in this statement.
    nodes: usize,
}

/// Limits that keep every recursive walk over the syntax tree (evaluation, policy checks, drop) far from the
/// thread stack limit. Overflowing the stack aborts the whole process, so a deeply nested statement from one
/// client would take every tenant down. Real queries use a few dozen nodes and a handful of nesting levels.
const MAX_NEST: usize = 64;
const MAX_NODES: usize = 2000;
const MAX_HOPS: usize = 64;

pub fn parse(src: &str) -> Result<Statement> {
    let mut p = P { toks: lex(src)?, i: 0, depth: 0, nodes: 0 };
    let st = p.statement()?;
    if p.peek() != &Tok::Eof {
        return p.err("unexpected trailing input");
    }
    Ok(st)
}

const RESERVED: [&str; 28] = [
    "MATCH", "WHERE", "RETURN", "CREATE", "SET", "DELETE", "CALL", "YIELD", "ORDER", "BY", "LIMIT", "SKIP", "AS", "AND", "OR", "NOT", "IN",
    "IS", "NULL", "TRUE", "FALSE", "DISTINCT", "ASC", "DESC", "OVER", "FROM", "TO", "HYPEREDGE",
];

impl P {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].0
    }
    fn pos(&self) -> usize {
        self.toks[self.i].1
    }
    fn err<T>(&self, m: &str) -> Result<T> {
        Err(Error::Parse { msg: m.to_string(), pos: self.pos() })
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.i].0.clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }
    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Tok::P(x) if *x == s)
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            self.err(&format!("expected '{s}'"))
        }
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Ident(x) if x.eq_ignore_ascii_case(k))
    }
    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_kw(&mut self, k: &str) -> Result<()> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            self.err(&format!("expected {k}"))
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(s)
            }
            _ => self.err("expected identifier"),
        }
    }
    fn name(&mut self) -> Result<String> {
        // identifier that is not a reserved word (variables, labels)
        if let Tok::Ident(s) = self.peek() {
            if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(s)) {
                return self.err("reserved word used as a name");
            }
        }
        self.ident()
    }
    fn uint(&mut self) -> Result<u64> {
        match self.bump() {
            Tok::Int(i) if i >= 0 => Ok(i as u64),
            _ => {
                self.i = self.i.saturating_sub(1);
                self.err("expected a non-negative integer")
            }
        }
    }

    fn statement(&mut self) -> Result<Statement> {
        let mut as_of = None;
        if self.eat_kw("AS") {
            self.expect_kw("OF")?;
            as_of = Some(self.uint()?);
        }
        let mut clauses = Vec::new();
        loop {
            if self.peek() == &Tok::Eof {
                break;
            }
            clauses.push(self.clause()?);
        }
        if clauses.is_empty() {
            return self.err("empty statement");
        }
        Ok(Statement { as_of, clauses })
    }

    fn clause(&mut self) -> Result<Clause> {
        let optional = self.eat_kw("OPTIONAL");
        if optional && !self.is_kw("MATCH") {
            return self.err("OPTIONAL must be followed by MATCH");
        }
        if self.eat_kw("MATCH") {
            let mut patterns = vec![self.pattern()?];
            while self.eat_sym(",") {
                patterns.push(self.pattern()?);
            }
            let wher = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
            Ok(Clause::Match { patterns, wher, optional })
        } else if self.eat_kw("WITH") {
            let Clause::Return(rc) = self.return_clause()? else { unreachable!() };
            let wher = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
            Ok(Clause::With { rc, wher })
        } else if self.eat_kw("UNWIND") {
            let expr = self.expr()?;
            self.expect_kw("AS")?;
            let var = self.name()?;
            Ok(Clause::Unwind { expr, var })
        } else if self.eat_kw("MERGE") {
            let path = self.path()?;
            if path.edges.iter().any(|e| e.varlen.is_some()) {
                return self.err("MERGE cannot use variable-length edges");
            }
            let (mut on_create, mut on_match) = (Vec::new(), Vec::new());
            while self.is_kw("ON") {
                self.bump();
                let create = if self.eat_kw("CREATE") {
                    true
                } else if self.eat_kw("MATCH") {
                    false
                } else {
                    return self.err("expected CREATE or MATCH after ON");
                };
                self.expect_kw("SET")?;
                let items = self.set_items()?;
                if create { on_create.extend(items) } else { on_match.extend(items) }
            }
            Ok(Clause::Merge { path, on_create, on_match })
        } else if self.eat_kw("REMOVE") {
            // REMOVE v.k is SET v.k = null
            let mut v = Vec::new();
            loop {
                let var = self.name()?;
                self.expect_sym(".")?;
                let key = self.name()?;
                v.push((var, key, Expr::Lit(Value::Null)));
                if !self.eat_sym(",") {
                    break;
                }
            }
            Ok(Clause::Set(v))
        } else if self.eat_kw("CREATE") {
            if self.eat_kw("INDEX") {
                if self.eat_kw("ON") {
                    self.expect_sym(":")?;
                    let label = self.name()?;
                    self.expect_sym("(")?;
                    let key = self.name()?;
                    self.expect_sym(")")?;
                    return Ok(Clause::CreatePropIndex { label, key });
                }
                // CREATE INDEX [name] FOR (v:Label) ON (v.key)
                if !self.is_kw("FOR") {
                    self.name()?;
                }
                self.expect_kw("FOR")?;
                self.expect_sym("(")?;
                let v = self.name()?;
                self.expect_sym(":")?;
                let label = self.name()?;
                self.expect_sym(")")?;
                self.expect_kw("ON")?;
                self.expect_sym("(")?;
                let v2 = self.name()?;
                if v2 != v {
                    return self.err("the index property must use the pattern variable");
                }
                self.expect_sym(".")?;
                let key = self.name()?;
                self.expect_sym(")")?;
                return Ok(Clause::CreatePropIndex { label, key });
            }
            if self.eat_kw("VECTOR") {
                self.expect_kw("INDEX")?;
                self.expect_kw("ON")?;
                self.expect_sym(":")?;
                let label = self.name()?;
                self.expect_sym("(")?;
                let key = self.name()?;
                self.expect_sym(")")?;
                self.expect_kw("DIM")?;
                let dim = self.uint()? as u32;
                let mut metric = Metric::Cosine;
                if self.eat_kw("METRIC") {
                    metric = match self.ident()?.to_ascii_lowercase().as_str() {
                        "cosine" => Metric::Cosine,
                        "l2" => Metric::L2,
                        _ => return self.err("metric must be cosine or l2"),
                    };
                }
                return Ok(Clause::CreateVectorIndex { label, key, dim, metric });
            }
            let mut items = vec![self.create_item()?];
            while self.eat_sym(",") {
                items.push(self.create_item()?);
            }
            Ok(Clause::Create(items))
        } else if self.eat_kw("SET") {
            Ok(Clause::Set(self.set_items()?))
        } else if self.eat_kw("DELETE") || (self.eat_kw("DETACH") && self.eat_kw("DELETE")) {
            let mut v = vec![self.name()?];
            while self.eat_sym(",") {
                v.push(self.name()?);
            }
            Ok(Clause::Delete(v))
        } else if self.eat_kw("RETURN") {
            self.return_clause()
        } else if self.eat_kw("CALL") {
            let mut name = self.ident()?;
            while self.eat_sym(".") {
                name.push('.');
                name.push_str(&self.ident()?);
            }
            self.expect_sym("(")?;
            let mut args = Vec::new();
            if !self.is_sym(")") {
                args.push(self.expr()?);
                while self.eat_sym(",") {
                    args.push(self.expr()?);
                }
            }
            self.expect_sym(")")?;
            let mut yields = Vec::new();
            if self.eat_kw("YIELD") && !self.eat_sym("*") {
                loop {
                    let col = self.name()?;
                    let alias = if self.eat_kw("AS") { self.name()? } else { col.clone() };
                    yields.push((col, alias));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            Ok(Clause::Call { name, args, yields })
        } else {
            self.err("expected MATCH, OPTIONAL MATCH, WITH, UNWIND, MERGE, CREATE, SET, REMOVE, DELETE, CALL or RETURN")
        }
    }

    /// `v.k = e` and `v += {k: e, ...}` items (the map form becomes one item per key, so every
    /// key is visible to the access-control check).
    fn set_items(&mut self) -> Result<Vec<(String, String, Expr)>> {
        let mut v = Vec::new();
        loop {
            let var = self.name()?;
            if self.eat_sym("+=") {
                let map = self.props_map()?;
                if map.is_empty() {
                    return self.err("SET v += needs a literal map with at least one key");
                }
                for (k, e) in map {
                    v.push((var.clone(), k, e));
                }
            } else {
                self.expect_sym(".")?;
                let key = self.name()?;
                self.expect_sym("=")?;
                v.push((var, key, self.expr()?));
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(v)
    }

    fn return_clause(&mut self) -> Result<Clause> {
        let distinct = self.eat_kw("DISTINCT");
        let mut items = Vec::new();
        loop {
            let start = self.i;
            let e = self.expr()?;
            let name = if self.eat_kw("AS") { self.name()? } else { self.render(start, self.i) };
            items.push((e, name));
            if !self.eat_sym(",") {
                break;
            }
        }
        let mut order = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let e = self.expr()?;
                let desc = if self.eat_kw("DESC") {
                    true
                } else {
                    self.eat_kw("ASC");
                    false
                };
                order.push((e, desc));
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") { Some(self.expr()?) } else { None };
        let limit = if self.eat_kw("LIMIT") { Some(self.expr()?) } else { None };
        Ok(Clause::Return(ReturnClause { distinct, items, order, skip, limit }))
    }

    fn render(&self, a: usize, b: usize) -> String {
        let mut s = String::new();
        for (t, _) in &self.toks[a..b] {
            match t {
                Tok::Ident(x) => s.push_str(x),
                Tok::Int(i) => s.push_str(&i.to_string()),
                Tok::Float(f) => s.push_str(&f.to_string()),
                Tok::Str(x) => s.push_str(&format!("'{x}'")),
                Tok::Param(x) => s.push_str(&format!("${x}")),
                Tok::P(x) => s.push_str(x),
                Tok::Eof => {}
            }
        }
        s
    }

    // ---- patterns

    fn props_map(&mut self) -> Result<Vec<(String, Expr)>> {
        let mut v = Vec::new();
        if self.eat_sym("{") {
            if !self.is_sym("}") {
                loop {
                    let k = self.ident()?;
                    self.expect_sym(":")?;
                    v.push((k, self.expr()?));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            self.expect_sym("}")?;
        }
        Ok(v)
    }

    fn node_pat(&mut self) -> Result<NodePat> {
        self.expect_sym("(")?;
        let mut n = NodePat::default();
        if matches!(self.peek(), Tok::Ident(_)) {
            n.var = Some(self.name()?.into());
        }
        if self.eat_sym(":") {
            n.label = Some(self.name()?);
        }
        n.props = self.props_map()?;
        self.expect_sym(")")?;
        Ok(n)
    }

    fn path(&mut self) -> Result<PathPat> {
        let mut nodes = vec![self.node_pat()?];
        let mut edges = Vec::new();
        loop {
            let left_in = if self.eat_sym("<-") {
                true
            } else if self.eat_sym("-") {
                false
            } else {
                break;
            };
            let (mut var, mut label, mut props, mut varlen) = (None, None, Vec::new(), None);
            if self.eat_sym("[") {
                if matches!(self.peek(), Tok::Ident(_)) {
                    var = Some(self.name()?.into());
                }
                if self.eat_sym(":") {
                    label = Some(self.name()?);
                }
                if self.eat_sym("*") {
                    let mut min = 1usize;
                    let mut max = 8usize;
                    if let Tok::Int(_) = self.peek() {
                        min = self.uint()? as usize;
                        max = min;
                    }
                    if self.eat_sym("..") {
                        max = if let Tok::Int(_) = self.peek() { self.uint()? as usize } else { 8 };
                    }
                    if min > max {
                        return self.err("variable-length minimum exceeds maximum");
                    }
                    varlen = Some((min, max));
                }
                props = self.props_map()?;
                self.expect_sym("]")?;
            }
            let right_out = if self.eat_sym("->") {
                true
            } else {
                self.expect_sym("-")?;
                false
            };
            let dir = match (left_in, right_out) {
                (true, true) => return self.err("an edge cannot point both ways"),
                (true, false) => Direction::In,
                (false, true) => Direction::Out,
                (false, false) => Direction::Any,
            };
            if edges.len() >= MAX_HOPS {
                return self.err("path pattern is too long (limit 64 hops)");
            }
            edges.push(EdgePat { var, label, props, dir, varlen });
            nodes.push(self.node_pat()?);
        }
        Ok(PathPat { nodes, edges })
    }

    fn pattern(&mut self) -> Result<Pattern> {
        if self.eat_kw("HYPEREDGE") {
            let var = self.name()?;
            let label = if self.eat_sym(":") { Some(self.name()?) } else { None };
            let props = self.props_map()?;
            return Ok(Pattern::Hyper { var, label, props });
        }
        Ok(Pattern::Path(self.path()?))
    }

    fn expr_list_paren(&mut self) -> Result<Vec<Expr>> {
        self.expect_sym("(")?;
        let mut v = Vec::new();
        if !self.is_sym(")") {
            v.push(self.expr()?);
            while self.eat_sym(",") {
                v.push(self.expr()?);
            }
        }
        self.expect_sym(")")?;
        Ok(v)
    }

    fn create_item(&mut self) -> Result<CreateItem> {
        if self.eat_kw("HYPEREDGE") {
            let var = if matches!(self.peek(), Tok::Ident(s) if !s.eq_ignore_ascii_case("OVER") && !s.eq_ignore_ascii_case("FROM")) {
                Some(self.name()?)
            } else {
                None
            };
            self.expect_sym(":")?;
            let label = self.name()?;
            let props = self.props_map()?;
            if self.eat_kw("OVER") {
                let tail = self.expr_list_paren()?;
                return Ok(CreateItem::Hyper { var, label, props, tail, head: vec![] });
            }
            self.expect_kw("FROM")?;
            let tail = self.expr_list_paren()?;
            self.expect_kw("TO")?;
            let head = self.expr_list_paren()?;
            return Ok(CreateItem::Hyper { var, label, props, tail, head });
        }
        Ok(CreateItem::Path(self.path()?))
    }

    // ---- expressions

    /// Count one expression node; refuse statements too large or too deep to process safely.
    fn tick(&mut self) -> Result<()> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return self.err("expression is too complex (more than 2000 operators and operands)");
        }
        Ok(())
    }

    pub fn expr(&mut self) -> Result<Expr> {
        self.tick()?;
        self.depth += 1;
        if self.depth > MAX_NEST {
            return self.err("expression is nested too deeply (limit 64 levels)");
        }
        let r = self.or();
        self.depth -= 1;
        r
    }
    fn or(&mut self) -> Result<Expr> {
        let mut l = self.and()?;
        while self.eat_kw("OR") {
            self.tick()?;
            l = Expr::Bin(Bin::Or, Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr> {
        let mut l = self.not()?;
        while self.eat_kw("AND") {
            self.tick()?;
            l = Expr::Bin(Bin::And, Box::new(l), Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr> {
        if self.eat_kw("NOT") {
            self.tick()?;
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }
    fn cmp(&mut self) -> Result<Expr> {
        let l = self.add()?;
        let op = if self.eat_sym("=") {
            Bin::Eq
        } else if self.eat_sym("<>") || self.eat_sym("!=") {
            Bin::Ne
        } else if self.eat_sym("<=") {
            Bin::Le
        } else if self.eat_sym(">=") {
            Bin::Ge
        } else if self.eat_sym("<") {
            Bin::Lt
        } else if self.eat_sym(">") {
            Bin::Gt
        } else if self.eat_kw("IN") {
            Bin::In
        } else if self.eat_kw("CONTAINS") {
            Bin::Contains
        } else if self.is_kw("STARTS") {
            self.bump();
            self.expect_kw("WITH")?;
            Bin::StartsWith
        } else if self.is_kw("ENDS") {
            self.bump();
            self.expect_kw("WITH")?;
            Bin::EndsWith
        } else if self.eat_kw("IS") {
            let neg = self.eat_kw("NOT");
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(l), neg));
        } else {
            return Ok(l);
        };
        self.tick()?;
        Ok(Expr::Bin(op, Box::new(l), Box::new(self.add()?)))
    }
    fn add(&mut self) -> Result<Expr> {
        let mut l = self.mul()?;
        loop {
            let op = if self.eat_sym("+") {
                Bin::Add
            } else if self.is_sym("-") && !self.looks_like_edge() {
                self.bump();
                Bin::Sub
            } else {
                return Ok(l);
            };
            self.tick()?;
            l = Expr::Bin(op, Box::new(l), Box::new(self.mul()?));
        }
    }
    fn looks_like_edge(&self) -> bool {
        false
    }
    fn mul(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = if self.eat_sym("*") {
                Bin::Mul
            } else if self.eat_sym("/") {
                Bin::Div
            } else if self.eat_sym("%") {
                Bin::Mod
            } else {
                return Ok(l);
            };
            self.tick()?;
            l = Expr::Bin(op, Box::new(l), Box::new(self.unary()?));
        }
    }
    fn unary(&mut self) -> Result<Expr> {
        if self.eat_sym("-") {
            self.tick()?;
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.pow()
    }
    /// `a ^ b` binds tighter than unary minus: `-2 ^ 2` is `-(2 ^ 2)`.
    fn pow(&mut self) -> Result<Expr> {
        let mut l = self.postfix()?;
        while self.eat_sym("^") {
            self.tick()?;
            let r = if self.eat_sym("-") { Expr::Neg(Box::new(self.postfix()?)) } else { self.postfix()? };
            l = Expr::Bin(Bin::Pow, Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.atom()?;
        loop {
            if self.is_sym(".") {
                self.tick()?;
                self.bump();
                let k = self.ident()?;
                e = Expr::Prop(Box::new(e), k);
            } else if self.is_sym("[") {
                self.tick()?;
                self.bump();
                let i = self.expr()?;
                self.expect_sym("]")?;
                e = Expr::Index(Box::new(e), Box::new(i));
            } else {
                return Ok(e);
            }
        }
    }
    fn atom(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.bump();
                Ok(Expr::Lit(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(Expr::Lit(Value::Float(f)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Lit(Value::Str(s)))
            }
            Tok::Param(p) => {
                self.bump();
                Ok(Expr::Param(p))
            }
            Tok::P("(") => {
                self.bump();
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::P("[") => {
                self.bump();
                let mut v = Vec::new();
                if !self.is_sym("]") {
                    v.push(self.expr()?);
                    while self.eat_sym(",") {
                        v.push(self.expr()?);
                    }
                }
                self.expect_sym("]")?;
                Ok(Expr::List(v))
            }
            Tok::Ident(id) => {
                let up = id.to_ascii_uppercase();
                match up.as_str() {
                    "TRUE" => {
                        self.bump();
                        return Ok(Expr::Lit(Value::Bool(true)));
                    }
                    "FALSE" => {
                        self.bump();
                        return Ok(Expr::Lit(Value::Bool(false)));
                    }
                    "NULL" => {
                        self.bump();
                        return Ok(Expr::Lit(Value::Null));
                    }
                    "CASE" => {
                        self.tick()?;
                        self.bump();
                        let operand = if self.is_kw("WHEN") { None } else { Some(Box::new(self.expr()?)) };
                        let mut whens = Vec::new();
                        while self.eat_kw("WHEN") {
                            let w = self.expr()?;
                            self.expect_kw("THEN")?;
                            whens.push((w, self.expr()?));
                        }
                        if whens.is_empty() {
                            return self.err("CASE needs at least one WHEN");
                        }
                        let els = if self.eat_kw("ELSE") { Some(Box::new(self.expr()?)) } else { None };
                        self.expect_kw("END")?;
                        return Ok(Expr::Case { operand, whens, els });
                    }
                    _ => {}
                }
                self.bump();
                if self.is_sym("(") {
                    self.bump();
                    let distinct = self.eat_kw("DISTINCT");
                    if self.eat_sym("*") {
                        self.expect_sym(")")?;
                        return Ok(Expr::Call { name: up.to_ascii_lowercase(), args: vec![], distinct: false, star: true });
                    }
                    let mut args = Vec::new();
                    if !self.is_sym(")") {
                        args.push(self.expr()?);
                        while self.eat_sym(",") {
                            args.push(self.expr()?);
                        }
                    }
                    self.expect_sym(")")?;
                    return Ok(Expr::Call { name: id.to_ascii_lowercase(), args, distinct, star: false });
                }
                if RESERVED.iter().any(|r| *r == up) {
                    self.i -= 1;
                    return self.err("unexpected keyword in expression");
                }
                Ok(Expr::Var(id))
            }
            _ => self.err("expected an expression"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pattern_and_return() {
        let s = parse("MATCH (a:Person {name:'x'})-[e:KNOWS*1..3]->(b) WHERE b.age > 3 AND NOT a.x IS NULL RETURN a.name, count(*) AS n ORDER BY n DESC LIMIT 5").unwrap();
        assert_eq!(s.clauses.len(), 2);
    }

    #[test]
    fn parses_hyperedge_create() {
        parse("CREATE HYPEREDGE m:Meeting {t:'x'} OVER (a, b, c)").unwrap();
        parse("CREATE HYPEREDGE :Reaction FROM (a, b) TO (c)").unwrap();
    }

    #[test]
    fn reports_position() {
        match parse("MATCH (a RETURN a") {
            Err(Error::Parse { pos, .. }) => assert!(pos > 0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arrows_and_minus() {
        parse("MATCH (a)<-[:T]-(b), (b)--(c), (c)-->(d) RETURN a.x - 1").unwrap();
    }
}
