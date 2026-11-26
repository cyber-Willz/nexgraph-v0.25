//! Column / label access policy, masking and small-cell suppression.
//!
//! Roles alone say *what kind* of statement a user may run; they say nothing about *which data* it
//! may touch. For health records that is the missing piece ("minimum necessary"). A [`Policy`] adds:
//!
//! * `deny` properties and `deny_labels`: a statement that mentions one anywhere (pattern, `WHERE`,
//!   `RETURN`, `ORDER BY`, `SET`, `CREATE`, or as a name passed to a procedure) is refused before it runs.
//!   Refusing the *mention* closes the inference channel as well as the read: you cannot filter on a
//!   column you cannot see.
//! * `mask` properties: may only be returned as a plain projection (`RETURN p.id`), where the value is
//!   replaced (redacted, keyed pseudonym, bucketed, top-coded). Using one in `WHERE`, `ORDER BY`, a
//!   pattern or a function argument is refused, so a masked value cannot be probed ("is patient X here?")
//!   by equality. `count(...)` over a masked property is allowed (a count reveals no value).
//! * `min_cell`: roles up to `min_cell_up_to` get aggregate answers only; every group must carry a
//!   `count(...)` column and groups smaller than `min_cell` are dropped. `collect`, `min` and `max`
//!   (which return individual values) are refused for those roles.
//!
//! Rules apply to roles up to and including `up_to` (a rule for `writer` covers `reader` and `writer`),
//! so `tenant_admin` can be left unrestricted (and audited).
//!
//! Property rules are `"prop"` (any label) or `"Label.prop"`. A reference whose label cannot be
//! determined statically matches conservatively. Names compare case-insensitively.
//!
//! What this does **not** do is stop differencing (two counts that differ by one person), linkage with
//! outside data, or a user who is allowed to see everything. See `SECURITY.md`.

use super::{hmac_sha256, hex, Role};
use crate::error::{Error, Result};
use crate::query::parser::{self, Clause, CreateItem, Expr, PathPat, Pattern, ReturnClause, Statement};
use crate::query::{Params, QueryResult};
use crate::value::Value;
use std::collections::{HashMap, HashSet};

/// How a masked property is shown.
#[derive(Clone, Debug, PartialEq)]
pub enum Mask {
    /// Replace with `"[redacted]"`.
    Redact,
    /// `HMAC-SHA256(policy key, "prop|value")`, first 12 hex digits: stable (joinable) but not reversible
    /// without the key.
    Pseudonym,
    /// Round numbers down to a multiple of `w` (`age` 47, w = 10 -> 40).
    Bucket(f64),
    /// Numbers above `n` become `n` (HIPAA Safe Harbor tops ages at 90).
    TopCode(f64),
}

#[derive(Clone, Debug, Default)]
pub struct Rule {
    /// The rule covers roles up to and including this one.
    pub up_to: Option<Role>,
    pub deny: Vec<String>,
    pub mask: Vec<(String, Mask)>,
    pub deny_labels: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub rules: Vec<Rule>,
    /// 0 = off. Otherwise groups backed by fewer records are dropped for roles up to `min_cell_up_to`.
    pub min_cell: u64,
    pub min_cell_up_to: Option<Role>,
    /// Key for [`Mask::Pseudonym`]. Must be set (and kept stable) if any pseudonym mask is used.
    pub pseudonym_key: Vec<u8>,
    /// Procedure name prefixes that aggregate-only roles may call without an aggregate `RETURN`.
    pub summary_procedures: Vec<String>,
}

/// What `apply` must do to a result.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    masks: Vec<(usize, String, Mask)>,
    count_col: Option<usize>,
    min_cell: u64,
    /// Property names the statement referenced (for the audit entry).
    pub props: Vec<String>,
}

/// What `apply` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub masked_cells: usize,
    pub suppressed_rows: usize,
}

const AGGS: [&str; 8] = ["count", "sum", "avg", "min", "max", "collect", "stdev", "median"];

fn is_agg(name: &str) -> bool {
    AGGS.contains(&name.to_ascii_lowercase().as_str())
}

#[derive(Clone)]
struct Pr {
    label: Option<String>,
    prop: String,
}

fn parse_ref(s: &str) -> Pr {
    match s.split_once('.') {
        Some((l, p)) if !l.is_empty() && !p.is_empty() => Pr { label: Some(l.to_ascii_lowercase()), prop: p.to_ascii_lowercase() },
        _ => Pr { label: None, prop: s.to_ascii_lowercase() },
    }
}

fn matches(r: &Pr, labels: &[String], prop: &str) -> bool {
    if !r.prop.eq_ignore_ascii_case(prop) {
        return false;
    }
    match &r.label {
        None => true,
        // unknown label (empty set) matches conservatively
        Some(l) => labels.is_empty() || labels.iter().any(|x| x == l),
    }
}

struct Effective {
    deny: Vec<Pr>,
    mask: Vec<(Pr, Mask)>,
    deny_labels: HashSet<String>,
}

impl Effective {
    fn is_empty(&self) -> bool {
        self.deny.is_empty() && self.mask.is_empty() && self.deny_labels.is_empty()
    }
    fn denied(&self, labels: &[String], prop: &str) -> bool {
        self.deny.iter().any(|r| matches(r, labels, prop))
    }
    fn masked(&self, labels: &[String], prop: &str) -> Option<&Mask> {
        self.mask.iter().find(|(r, _)| matches(r, labels, prop)).map(|(_, m)| m)
    }
    /// Is `name` (a bare token from a procedure argument) a protected property or label?
    fn token_protected(&self, name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        if self.deny_labels.contains(&n) {
            return true;
        }
        let hit = |r: &Pr| {
            if r.prop == n {
                return true;
            }
            match &r.label {
                Some(l) => n == format!("{l}.{}", r.prop),
                None => false,
            }
        };
        self.deny.iter().any(hit) || self.mask.iter().any(|(r, _)| hit(r))
    }
}

impl Policy {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.min_cell == 0
    }

    fn effective(&self, role: Role) -> Effective {
        let mut e = Effective { deny: vec![], mask: vec![], deny_labels: HashSet::new() };
        for r in &self.rules {
            if r.up_to.map(|u| role <= u).unwrap_or(true) {
                e.deny.extend(r.deny.iter().map(|s| parse_ref(s)));
                e.mask.extend(r.mask.iter().map(|(s, m)| (parse_ref(s), m.clone())));
                e.deny_labels.extend(r.deny_labels.iter().map(|s| s.to_ascii_lowercase()));
            }
        }
        e
    }

    fn aggregate_only(&self, role: Role) -> bool {
        self.min_cell > 0 && self.min_cell_up_to.map(|u| role <= u).unwrap_or(true)
    }

    /// Is this label hidden from `role`? (BI export, catalogs.)
    pub fn label_denied(&self, role: Role, label: &str) -> bool {
        self.effective(role).deny_labels.contains(&label.to_ascii_lowercase())
    }

    /// Is a column of a label table hidden from `role`?
    pub fn column_denied(&self, role: Role, label: &str, prop: &str) -> bool {
        let e = self.effective(role);
        e.denied(&[label.to_ascii_lowercase()], prop)
    }

    /// The mask for a column of a label table, if any.
    pub fn column_mask(&self, role: Role, label: &str, prop: &str) -> Option<Mask> {
        self.effective(role).masked(&[label.to_ascii_lowercase()], prop).cloned()
    }

    /// True when `role` is restricted at all (whole-table JSON exports must then be refused).
    pub fn restricts(&self, role: Role) -> bool {
        !self.effective(role).is_empty() || self.aggregate_only(role)
    }

    pub fn aggregate_only_for(&self, role: Role) -> bool {
        self.aggregate_only(role)
    }

    /// Mask one value (`prop` and `label` only feed the pseudonym so equal values in different columns differ).
    pub fn mask_value(&self, m: &Mask, prop: &str, v: &Value) -> Value {
        match (m, v) {
            (_, Value::Null) => Value::Null,
            (Mask::Redact, _) => Value::Str("[redacted]".into()),
            (Mask::Pseudonym, v) => {
                let text = match v {
                    Value::Str(s) => s.clone(),
                    Value::Int(i) => i.to_string(),
                    Value::Float(f) => f.to_string(),
                    Value::Bool(b) => b.to_string(),
                    other => format!("{other:?}"),
                };
                let mac = hmac_sha256(&self.pseudonym_key, format!("{}|{text}", prop.to_ascii_lowercase()).as_bytes());
                Value::Str(format!("p_{}", &hex(&mac)[..12]))
            }
            (Mask::Bucket(w), Value::Int(i)) if *w >= 1.0 => Value::Int(((*i as f64 / w).floor() * w) as i64),
            (Mask::Bucket(w), Value::Float(f)) if *w > 0.0 => Value::Float((f / w).floor() * w),
            (Mask::TopCode(n), Value::Int(i)) => Value::Int((*i).min(*n as i64)),
            (Mask::TopCode(n), Value::Float(f)) => Value::Float(f.min(*n)),
            // a numeric mask on a non-number: never pass the raw value through
            _ => Value::Str("[redacted]".into()),
        }
    }

    /// Check a statement for `role`. Returns what to do to its result, or `Denied`.
    pub fn check(&self, role: Role, st: &Statement, params: &Params) -> Result<Plan> {
        let eff = self.effective(role);
        let agg_only = self.aggregate_only(role);
        if eff.is_empty() && !agg_only {
            return Ok(Plan::default());
        }
        let mut cx = Cx { eff: &eff, vars: HashMap::new(), props: vec![], masks: vec![] };
        // pass 1: variable -> labels
        for c in &st.clauses {
            match c {
                Clause::Match { patterns, .. } => {
                    for p in patterns {
                        match p {
                            Pattern::Path(pp) => cx.bind_path(pp),
                            Pattern::Hyper { var, label, .. } => cx.bind(var, label.as_deref()),
                        }
                    }
                }
                Clause::Merge { path, .. } => cx.bind_path(path),
                // An alias may carry nodes through WITH / UNWIND: it inherits the labels of every variable the
                // expression mentions (a union, so it can only make the checks stricter). No variable at all
                // leaves the label set empty, which matches every rule.
                Clause::With { rc, .. } => {
                    for (e, alias) in &rc.items {
                        let mut vs = vec![];
                        expr_vars(e, &mut vs);
                        for v in vs {
                            for l in cx.labels_of(&v) {
                                cx.bind(alias, Some(&l));
                            }
                        }
                    }
                }
                Clause::Unwind { expr, var } => {
                    let mut vs = vec![];
                    expr_vars(expr, &mut vs);
                    for v in vs {
                        for l in cx.labels_of(&v) {
                            cx.bind(var, Some(&l));
                        }
                    }
                }
                Clause::Create(items) => {
                    for it in items {
                        match it {
                            CreateItem::Path(pp) => cx.bind_path(pp),
                            CreateItem::Hyper { var, label, .. } => {
                                if let Some(v) = var {
                                    cx.bind(v, Some(label));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // pass 2: check every mention
        let mut ret: Option<&ReturnClause> = None;
        let mut has_call = false;
        let mut summary_call = true;
        for c in &st.clauses {
            match c {
                Clause::With { rc, wher } => {
                    // Anything projected by WITH feeds later clauses, so it can be neither denied nor masked:
                    // a mask is applied to final output columns only.
                    for (e, _) in &rc.items {
                        cx.expr(e, Site::Other)?;
                    }
                    for (e, _) in &rc.order {
                        cx.expr(e, Site::Other)?;
                    }
                    for e in rc.skip.iter().chain(rc.limit.iter()).chain(wher.iter()) {
                        cx.expr(e, Site::Other)?;
                    }
                }
                Clause::Unwind { expr, .. } => cx.expr(expr, Site::Other)?,
                Clause::Merge { path, on_create, on_match } => {
                    cx.check_path(path)?;
                    for (var, key, e) in on_create.iter().chain(on_match.iter()) {
                        let labels = cx.labels_of(var);
                        cx.prop_site(&labels, key, Site::Other)?;
                        cx.expr(e, Site::Other)?;
                    }
                }
                Clause::Match { patterns, wher, .. } => {
                    for p in patterns {
                        match p {
                            Pattern::Path(pp) => cx.check_path(pp)?,
                            Pattern::Hyper { label, props, .. } => {
                                cx.check_label(label.as_deref())?;
                                cx.check_pattern_props(label.as_deref(), props)?;
                            }
                        }
                    }
                    if let Some(w) = wher {
                        cx.expr(w, Site::Other)?;
                    }
                }
                Clause::Create(items) => {
                    for it in items {
                        match it {
                            CreateItem::Path(pp) => cx.check_path(pp)?,
                            CreateItem::Hyper { label, props, tail, head, .. } => {
                                cx.check_label(Some(label))?;
                                cx.check_pattern_props(Some(label), props)?;
                                for e in tail.iter().chain(head.iter()) {
                                    cx.expr(e, Site::Other)?;
                                }
                            }
                        }
                    }
                }
                Clause::Set(items) => {
                    for (var, key, e) in items {
                        let labels = cx.labels_of(var);
                        cx.prop_site(&labels, key, Site::Other)?;
                        cx.expr(e, Site::Other)?;
                    }
                }
                Clause::Delete(_) => {}
                Clause::Call { name, args, .. } => {
                    has_call = true;
                    if !self.summary_procedures.iter().any(|p| name.to_ascii_lowercase().starts_with(&p.to_ascii_lowercase())) {
                        summary_call = false;
                    }
                    for a in args {
                        cx.expr(a, Site::Other)?;
                        cx.call_arg_names(a, params)?;
                    }
                }
                Clause::CreatePropIndex { label, key } | Clause::CreateVectorIndex { label, key, .. } => {
                    cx.check_label(Some(label))?;
                    cx.prop_site(&[label.to_ascii_lowercase()], key, Site::Other)?;
                }
                Clause::Return(r) => ret = Some(r),
            }
        }
        // Small-cell suppression counts result rows, so a query shape that multiplies rows can lift one person's
        // group over the threshold (MATCH (a {age: 93}), (b:Other) ... count(*); UNWIND range(1, 50) ...).
        // Count-restricted roles therefore get one MATCH with one pattern, no row-multiplying clauses, and for
        // patterns with edges a count of DISTINCT nodes rather than of rows.
        if agg_only {
            let mut matches = st.clauses.iter().filter_map(|c| if let Clause::Match { patterns, optional, .. } = c { Some((patterns, *optional)) } else { None });
            let first = matches.next();
            let shape_ok = matches.next().is_none()
                && first.map_or(true, |(p, opt)| p.len() == 1 && !opt)
                && !st.clauses.iter().any(|c| matches!(c, Clause::Unwind { .. } | Clause::With { .. } | Clause::Merge { .. }))
                && !(has_call && first.is_some());
            if !shape_ok {
                return Err(Error::Denied("this role may only run aggregate queries over one MATCH with one pattern (no UNWIND, WITH, OPTIONAL MATCH, joins or CALL after MATCH): such shapes can inflate counts past the small-group threshold".into()));
            }
        }
        let multi_hop = st.clauses.iter().any(|c| match c {
            Clause::Match { patterns, .. } => patterns.iter().any(|p| matches!(p, Pattern::Path(pp) if !pp.edges.is_empty())),
            _ => false,
        });
        let mut plan = Plan { props: vec![], ..Default::default() };
        if let Some(r) = ret {
            for (i, (e, _alias)) in r.items.iter().enumerate() {
                cx.projection(e, i)?;
            }
            for (e, _) in &r.order {
                cx.expr(e, Site::Other)?;
            }
            for e in r.skip.iter().chain(r.limit.iter()) {
                cx.expr(e, Site::Other)?;
            }
            if agg_only {
                let mut count_col = None;
                let mut any_agg = false;
                for (i, (e, _)) in r.items.iter().enumerate() {
                    if let Expr::Call { name, .. } = e {
                        let n = name.to_ascii_lowercase();
                        if is_agg(&n) {
                            any_agg = true;
                            if matches!(n.as_str(), "collect" | "min" | "max") {
                                return Err(Error::Denied(format!("{n}() returns individual values and is not available to this role")));
                            }
                            if n == "count" && count_col.is_none() {
                                // with edges in the pattern, a row is not a person: count distinct nodes
                                if multi_hop {
                                    let ok = matches!(e, Expr::Call { distinct: true, args, .. } if matches!(args.as_slice(), [Expr::Var(v)] if cx.vars.contains_key(v)));
                                    if !ok {
                                        return Err(Error::Denied("with relationships in the pattern, count DISTINCT a node variable (count(DISTINCT a)); count(*) counts paths, not entities".into()));
                                    }
                                }
                                count_col = Some(i);
                            }
                        }
                    }
                    if contains_nested_agg_forbidden(e) {
                        return Err(Error::Denied("collect/min/max are not available to this role".into()));
                    }
                }
                if !any_agg && !(has_call && summary_call) {
                    return Err(Error::Denied("this role may only run aggregate queries (RETURN count(*), ... grouped by what you need)".into()));
                }
                if any_agg {
                    match count_col {
                        Some(c) => {
                            plan.count_col = Some(c);
                            plan.min_cell = self.min_cell;
                        }
                        None => return Err(Error::Denied(format!("aggregate queries must include count(*) so groups smaller than {} can be suppressed", self.min_cell))),
                    }
                }
            }
        } else if agg_only && !(has_call && summary_call) && !st.has_writes() {
            return Err(Error::Denied("this role may only run aggregate queries".into()));
        } else if agg_only && st.has_writes() {
            // writes return nothing to suppress
        }
        plan.masks = cx.masks;
        let mut seen = HashSet::new();
        plan.props = cx.props.into_iter().filter(|p| seen.insert(p.clone())).collect();
        Ok(plan)
    }

    /// Apply masks and small-cell suppression to a result. Fail closed: if the plan expects a column
    /// that is missing, the whole result is withheld.
    pub fn apply(&self, plan: &Plan, mut r: QueryResult) -> Result<(QueryResult, Applied)> {
        let mut a = Applied::default();
        if plan.masks.iter().any(|(_, _, m)| *m == Mask::Pseudonym) && self.pseudonym_key.len() < 16 {
            return Err(Error::Denied("policy misconfigured: pseudonym masks need a key of at least 16 bytes".into()));
        }
        for (col, prop, m) in &plan.masks {
            if *col >= r.columns.len() {
                return Err(Error::Denied("policy could not be applied to this result".into()));
            }
            for row in r.rows.iter_mut() {
                let masked = self.mask_value(m, prop, &row[*col]);
                if masked != row[*col] {
                    a.masked_cells += 1;
                }
                row[*col] = masked;
            }
        }
        if let Some(c) = plan.count_col {
            if c >= r.columns.len() {
                return Err(Error::Denied("policy could not be applied to this result".into()));
            }
            let before = r.rows.len();
            let k = plan.min_cell as i64;
            r.rows.retain(|row| matches!(&row[c], Value::Int(n) if *n >= k));
            a.suppressed_rows = before - r.rows.len();
        }
        Ok((r, a))
    }
}

fn contains_nested_agg_forbidden(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => {
            matches!(name.to_ascii_lowercase().as_str(), "collect" | "min" | "max") || args.iter().any(contains_nested_agg_forbidden)
        }
        Expr::Prop(b, _) | Expr::Not(b) | Expr::Neg(b) | Expr::IsNull(b, _) => contains_nested_agg_forbidden(b),
        Expr::List(v) => v.iter().any(contains_nested_agg_forbidden),
        Expr::Bin(_, a, b) | Expr::Index(a, b) => contains_nested_agg_forbidden(a) || contains_nested_agg_forbidden(b),
        Expr::Case { operand, whens, els } => {
            operand.iter().chain(els.iter()).any(|e| contains_nested_agg_forbidden(e)) || whens.iter().any(|(w, t)| contains_nested_agg_forbidden(w) || contains_nested_agg_forbidden(t))
        }
        _ => false,
    }
}

/// Variables mentioned in an expression.
fn expr_vars(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Var(v) => out.push(v.clone()),
        Expr::Prop(b, _) | Expr::Not(b) | Expr::Neg(b) | Expr::IsNull(b, _) => expr_vars(b, out),
        Expr::List(v) => v.iter().for_each(|x| expr_vars(x, out)),
        Expr::Call { args, .. } => args.iter().for_each(|x| expr_vars(x, out)),
        Expr::Bin(_, a, b) | Expr::Index(a, b) => {
            expr_vars(a, out);
            expr_vars(b, out);
        }
        Expr::Case { operand, whens, els } => {
            for x in operand.iter().chain(els.iter()) {
                expr_vars(x, out);
            }
            for (w, t) in whens {
                expr_vars(w, out);
                expr_vars(t, out);
            }
        }
        Expr::Lit(_) | Expr::Param(_) => {}
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Site {
    /// Anywhere except as a direct projection.
    Other,
    /// Inside `count(...)`: a count reveals no value, so masked (not denied) properties are fine.
    InCount,
}

struct Cx<'a> {
    eff: &'a Effective,
    vars: HashMap<String, Vec<String>>,
    props: Vec<String>,
    masks: Vec<(usize, String, Mask)>,
}

impl Cx<'_> {
    fn bind(&mut self, var: &str, label: Option<&str>) {
        let e = self.vars.entry(var.to_string()).or_default();
        if let Some(l) = label {
            let l = l.to_ascii_lowercase();
            if !e.contains(&l) {
                e.push(l);
            }
        }
    }
    fn bind_path(&mut self, p: &PathPat) {
        for n in &p.nodes {
            if let Some(v) = &n.var {
                self.bind(v, n.label.as_deref());
            }
        }
        for e in &p.edges {
            if let Some(v) = &e.var {
                self.bind(v, e.label.as_deref());
            }
        }
    }
    fn labels_of(&self, var: &str) -> Vec<String> {
        self.vars.get(var).cloned().unwrap_or_default()
    }
    fn check_label(&self, l: Option<&str>) -> Result<()> {
        if let Some(l) = l {
            if self.eff.deny_labels.contains(&l.to_ascii_lowercase()) {
                return Err(Error::Denied(format!("label {l} is not available to your role")));
            }
        }
        Ok(())
    }
    fn check_path(&mut self, p: &PathPat) -> Result<()> {
        for n in &p.nodes {
            self.check_label(n.label.as_deref())?;
            self.check_pattern_props(n.label.as_deref(), &n.props)?;
        }
        for e in &p.edges {
            self.check_label(e.label.as_deref())?;
            self.check_pattern_props(e.label.as_deref(), &e.props)?;
        }
        Ok(())
    }
    fn check_pattern_props(&mut self, label: Option<&str>, props: &[(String, Expr)]) -> Result<()> {
        let labels: Vec<String> = label.map(|l| vec![l.to_ascii_lowercase()]).unwrap_or_default();
        for (k, e) in props {
            self.prop_site(&labels, k, Site::Other)?;
            self.expr(e, Site::Other)?;
        }
        Ok(())
    }
    /// A property mention that is not a plain projection.
    fn prop_site(&mut self, labels: &[String], prop: &str, site: Site) -> Result<()> {
        self.props.push(prop.to_ascii_lowercase());
        if self.eff.denied(labels, prop) {
            return Err(Error::Denied(format!("property {prop} is not available to your role")));
        }
        if site == Site::Other && self.eff.masked(labels, prop).is_some() {
            return Err(Error::Denied(format!("property {prop} is masked for your role: it can be returned, but not filtered, sorted or used in expressions")));
        }
        Ok(())
    }
    fn expr(&mut self, e: &Expr, site: Site) -> Result<()> {
        match e {
            Expr::Lit(_) | Expr::Param(_) | Expr::Var(_) => Ok(()),
            Expr::Prop(base, name) => {
                let labels = match &**base {
                    Expr::Var(v) => self.labels_of(v),
                    other => {
                        self.expr(other, site)?;
                        vec![]
                    }
                };
                self.prop_site(&labels, name, site)
            }
            Expr::Call { name, args, .. } => {
                let inner = if name.eq_ignore_ascii_case("count") { Site::InCount } else { Site::Other };
                for a in args {
                    self.expr(a, inner)?;
                }
                // keys(n) lists property names: the schema is not data, but it would list a denied name
                // to a user who cannot see the value; keep it allowed (names only).
                Ok(())
            }
            Expr::List(v) => v.iter().try_for_each(|x| self.expr(x, site)),
            Expr::Not(x) | Expr::Neg(x) | Expr::IsNull(x, _) => self.expr(x, site),
            Expr::Bin(_, a, b) | Expr::Index(a, b) => {
                self.expr(a, site)?;
                self.expr(b, site)
            }
            Expr::Case { operand, whens, els } => {
                for e in operand.iter().chain(els.iter()) {
                    self.expr(e, site)?;
                }
                for (w, t) in whens {
                    self.expr(w, site)?;
                    self.expr(t, site)?;
                }
                Ok(())
            }
        }
    }
    /// A RETURN item. A bare `var.prop` of a masked property is recorded for masking; anything else is
    /// checked like any other expression.
    fn projection(&mut self, e: &Expr, col: usize) -> Result<()> {
        if let Expr::Prop(base, name) = e {
            if let Expr::Var(v) = &**base {
                let labels = self.labels_of(v);
                self.props.push(name.to_ascii_lowercase());
                if self.eff.denied(&labels, name) {
                    return Err(Error::Denied(format!("property {name} is not available to your role")));
                }
                if let Some(m) = self.eff.masked(&labels, name) {
                    self.masks.push((col, name.clone(), m.clone()));
                }
                return Ok(());
            }
        }
        self.expr(e, Site::Other)
    }
    /// Names handed to a procedure (`uw.audit('Patient', 'died', 'age,weight')`) may be properties or
    /// labels; refuse any argument that names something protected. `$param` arguments are resolved.
    fn call_arg_names(&self, e: &Expr, params: &Params) -> Result<()> {
        let mut texts: Vec<String> = vec![];
        collect_strings(e, params, &mut texts);
        for t in texts {
            for tok in t.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')).filter(|s| !s.is_empty()) {
                if self.eff.token_protected(tok) {
                    return Err(Error::Denied(format!("{tok} is not available to your role (named in a procedure argument)")));
                }
                // `Label.prop` -> also check the property part alone against `prop`-only rules
                if let Some((_, p)) = tok.split_once('.') {
                    if self.eff.token_protected(p) {
                        return Err(Error::Denied(format!("{p} is not available to your role (named in a procedure argument)")));
                    }
                }
            }
            // free-text conditions such as "age > 80 AND hospitalid = 3": the tokenizer above covers them.
        }
        Ok(())
    }
}

fn collect_strings(e: &Expr, params: &Params, out: &mut Vec<String>) {
    match e {
        Expr::Lit(Value::Str(s)) => out.push(s.clone()),
        Expr::Lit(Value::List(l)) => {
            for v in l {
                if let Value::Str(s) = v {
                    out.push(s.clone());
                }
            }
        }
        Expr::Param(p) => match params.get(p) {
            Some(Value::Str(s)) => out.push(s.clone()),
            Some(Value::List(l)) => {
                for v in l {
                    if let Value::Str(s) = v {
                        out.push(s.clone());
                    }
                }
            }
            _ => {}
        },
        Expr::List(v) => v.iter().for_each(|x| collect_strings(x, params, out)),
        Expr::Call { args, .. } => args.iter().for_each(|x| collect_strings(x, params, out)),
        Expr::Bin(_, a, b) | Expr::Index(a, b) => {
            collect_strings(a, params, out);
            collect_strings(b, params, out);
        }
        Expr::Case { operand, whens, els } => {
            for e in operand.iter().chain(els.iter()) {
                collect_strings(e, params, out);
            }
            for (w, t) in whens {
                collect_strings(w, params, out);
                collect_strings(t, params, out);
            }
        }
        Expr::Prop(b, _) | Expr::Not(b) | Expr::Neg(b) | Expr::IsNull(b, _) => collect_strings(b, params, out),
        _ => {}
    }
}

/// Parse `text` and check it. Convenience for callers that only have text.
pub fn check_text(p: &Policy, role: Role, text: &str, params: &Params) -> Result<Plan> {
    let st = parser::parse(text)?;
    p.check(role, &st, params)
}

#[cfg(feature = "ingest")]
impl Policy {
    /// Load a policy from JSON:
    /// ```json
    /// {"min_cell": 11, "min_cell_up_to": "writer", "pseudonym_key_hex": "00ff...",
    ///  "summary_procedures": ["uw.gains"],
    ///  "rules": [{"up_to": "writer", "deny": ["Patient.dob"], "deny_labels": ["Note"],
    ///             "mask": {"uniquepid": "pseudonym", "age": "top_code:89", "weight": "bucket:5", "name": "redact"}}]}
    /// ```
    pub fn from_json(text: &str) -> Result<Policy> {
        use serde_json::Value as J;
        let j: J = serde_json::from_str(text).map_err(|e| Error::Invalid(format!("policy JSON: {e}")))?;
        let role = |v: &J| -> Result<Option<Role>> {
            match v {
                J::Null => Ok(None),
                J::String(s) => Role::parse(s).map(Some),
                _ => Err(Error::Invalid("policy: role must be a string".into())),
            }
        };
        let strs = |v: &J| -> Result<Vec<String>> {
            match v {
                J::Null => Ok(vec![]),
                J::Array(a) => a.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| Error::Invalid("policy: expected strings".into()))).collect(),
                _ => Err(Error::Invalid("policy: expected an array of strings".into())),
            }
        };
        let mut p = Policy { min_cell: j["min_cell"].as_u64().unwrap_or(0), min_cell_up_to: role(&j["min_cell_up_to"])?, summary_procedures: strs(&j["summary_procedures"])?, ..Default::default() };
        if let Some(h) = j["pseudonym_key_hex"].as_str() {
            if h.len() % 2 != 0 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Invalid("policy: pseudonym_key_hex must be hex".into()));
            }
            p.pseudonym_key = (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect();
        }
        if let J::Array(rules) = &j["rules"] {
            for r in rules {
                let mut rule = Rule { up_to: role(&r["up_to"])?, deny: strs(&r["deny"])?, deny_labels: strs(&r["deny_labels"])?, ..Default::default() };
                if let J::Object(m) = &r["mask"] {
                    for (k, v) in m {
                        let s = v.as_str().ok_or_else(|| Error::Invalid("policy: mask values are strings".into()))?;
                        let mask = match s.split_once(':') {
                            None if s == "redact" => Mask::Redact,
                            None if s == "pseudonym" => Mask::Pseudonym,
                            Some(("bucket", n)) => Mask::Bucket(n.parse().map_err(|_| Error::Invalid(format!("policy: bad bucket width {n:?}")))?),
                            Some(("top_code", n)) => Mask::TopCode(n.parse().map_err(|_| Error::Invalid(format!("policy: bad top_code {n:?}")))?),
                            _ => return Err(Error::Invalid(format!("policy: unknown mask {s:?} (redact, pseudonym, bucket:N, top_code:N)"))),
                        };
                        if mask == Mask::Pseudonym && p.pseudonym_key.len() < 16 {
                            return Err(Error::Invalid("policy: pseudonym masks need pseudonym_key_hex of at least 16 bytes".into()));
                        }
                        rule.mask.push((k.clone(), mask));
                    }
                }
                p.rules.push(rule);
            }
        }
        Ok(p)
    }
}
