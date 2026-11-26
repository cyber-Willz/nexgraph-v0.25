//! # nexgraph
//!
//! An embedded **property hypergraph database** in Rust.
//!
//! * **Data model**: nodes with labels and properties; edges are *directed
//!   hyperedges* (`tail -> head` sets) with a label, weight and properties.
//!   A binary edge is the 1-to-1 case; an undirected hyperedge has an empty head.
//! * **Durability**: checksummed write-ahead log, atomic checkpoints, torn-tail
//!   recovery, **or** a PostgreSQL schema (feature `postgres`): commit log,
//!   chunked checkpoints, single-writer advisory lock, read-only replicas and a
//!   transactionally maintained relational mirror queryable with plain SQL
//!   ([`Database::open_postgres`]).
//! * **Concurrency**: MVCC snapshot isolation. One writer at a time, any number of
//!   readers; readers never block each other and always see a consistent state.
//! * **Time travel**: every commit has a timestamp; `AS OF n` queries and
//!   [`Database::read_at`] read historical states (until the next checkpoint compacts history).
//! * **Indexes**: label index (always), property equality indexes, HNSW vector indexes.
//! * **Query language**: NQL, a Cypher-style language with hyperedge support (see [`query`]).
//! * **Spectral analytics** (feature `spectral`, on by default): Fiedler vector,
//!   spectral clustering, Laplacian spectrum and embeddings of the live
//!   hypergraph through `CALL spectral.*`, powered by the `spectral_hypergraph`
//!   crate, plus PageRank, components, shortest paths and Dijkstra.
//!
//! * **Analytics** (always on, no dependencies): `algo.degree/betweenness/closeness/kcore/triangles/scc/
//!   articulation/louvain/similarity` and `stats.describe/outliers/histogram/correlation/timeseries/spikes`.
//! * **Land registry**: `land.conflicts/rule_check/hotspots/valuation/survey_check` for parcel and title records (competing owners, status rule
//!   violations, dispute hotspots, valuation outliers, survey-number consistency), see [`land`].
//! * **ALM**: `alm.position/liability_profile/duration_gap/rate_shock/cashflow_ladder/krd_gap/scenario_risk` for asset-liability
//!   management (duration gap, rate shocks, cash-flow matching, key-rate DV01, stochastic-scenario VaR), see [`alm`].
//! * **GAAP ledger**: [`gaap`] is a double-entry general ledger (balanced, immutable, gapless entries; period close; reversals) with trial balance,
//!   income statement, retained earnings, classified balance sheet, cash-flow statement, T-accounts and an integrity audit as `CALL gaap.*`,
//!   following the reference GAAP PostgreSQL schema. Journal entries are `:JournalEntry` nodes with `:LINE` edges plus one `:POSTING` hyperedge each.
//! * **BI / Power BI**: [`bi`] flattens a snapshot into typed tables (CSV/JSON, Power Query scripts); the server serves them at `/v1/bi/*`.
//! * **Cyber** : `cyber.reachable/attack_paths/chokepoints/propagate_risk/fanout/beaconing/volume/ioc`,
//!   snapshot [`drift`] detection, and (feature `ingest`) CSV / Suricata EVE / Zeek loaders in [`ingest`].
//!
//! ```
//! use nexgraph::Database;
//! let db = Database::in_memory();
//! db.query("CREATE (a:Person {name:'Ada'}), (b:Person {name:'Bob'}), (a)-[:KNOWS]->(b)").unwrap();
//! let r = db.query("MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name").unwrap();
//! assert_eq!(r.rows.len(), 1);
//! ```

pub mod algo;
pub mod alm;
pub mod land;
pub mod fin;
pub mod gaap;
pub mod swarm;
pub mod uw;
pub mod analytics;
pub mod bi;
pub mod cyber;
pub mod killchain;
#[cfg(feature = "ingest")]
pub mod ingest;
pub(crate) mod netgraph;
#[cfg(feature = "nonbacktracking")]
pub mod community;
#[cfg(feature = "crypto")]
pub mod crypto;
#[cfg(all(feature = "crypto", feature = "ingest"))]
pub mod deid;
#[cfg(all(feature = "crypto", feature = "ingest"))]
pub mod profile;
pub mod db;
pub mod error;
pub mod hnsw;
#[cfg(feature = "spectral")]
pub(crate) mod kmeans;
pub mod model;
#[cfg(feature = "postgres")]
pub mod pg;
#[cfg(feature = "security")]
pub mod security;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "postgres")]
pub mod tls;
pub mod physics;
pub mod query;
pub mod store;
pub mod timeutil;
pub mod value;

pub use cyber::{drift, Drift};
pub use db::{Database, SyncMode, Txn};
pub use error::{Error, Result};
pub use model::Metric;
#[cfg(feature = "postgres")]
pub use pg::{MirrorReport, MirrorStats, PgOptions, PgSessionInfo, Refresh};
#[cfg(feature = "postgres")]
pub use tls::PgTls;
pub use query::{Params, QueryResult};
pub use store::{Direction, View};
pub use value::{props, Id, Props, Value};
