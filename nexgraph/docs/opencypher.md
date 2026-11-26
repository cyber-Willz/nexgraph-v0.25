# openCypher compatibility (v0.21)

NQL is a Cypher dialect. This release closes most of the gap with openCypher. It is **not** a complete implementation, and it is **not GQL (ISO 39075)**. The measure below is a corpus of 59 common queries written for this project (`tests/cypher/corpus.txt`), not the official openCypher TCK: **49 of 59 run** (44% at the start of this work). `tests/cypher_semantics.rs` checks the exact results of the new features.

## Added
`WITH` (with aggregation, ORDER BY/SKIP/LIMIT, WHERE) · `OPTIONAL MATCH` · `UNWIND` · `MERGE` (+ `ON CREATE SET` / `ON MATCH SET`) · `REMOVE v.k` · `SET v += {…}` · `CASE` (simple and searched) · list indexing `l[i]` · `^` · `CREATE INDEX [name] FOR (v:L) ON (v.k)`.
Functions: `toLower toUpper toInteger toBoolean round floor ceil sign exp log log10 exists timestamp trim ltrim rtrim substring left right replace split reverse range head last tail`.

## Not supported (fails with a clear error)
`UNION` · `EXISTS { … }` and pattern predicates · path variables (`p = (…)`) and `length(p)` · list comprehensions · aggregates nested inside expressions (`head(collect(x))`; use `WITH`) · `SET v:Label` · `properties(v)` · temporal functions (`date()`) · `CREATE CONSTRAINT` · `SET v = {…}` · `WITH *`.

## Differences to know
* `MERGE` reads from the statement's start-of-query snapshot; it remembers what it created itself, so `UNWIND ['a','a'] AS k MERGE (:T {k:k})` creates one node. A `null` property in a MERGE pattern is an error.
* `OPTIONAL MATCH … WHERE` filters the optional match, not the row (as in Cypher).
* `^` always returns a float; `/` on two integers is integer division (as in Cypher).
* `range()` is capped at 1,000,000 values.

## Access control and the new clauses
* A denied or masked property cannot be carried through `WITH`, `UNWIND`, `CASE` or list indexing: anything a `WITH` projects is checked as an ordinary expression, and an alias inherits the labels of every variable it mentions.
* `SET v += {…}` and `REMOVE` are expanded to one `SET` item per key before the check, so every key is checked. `MERGE` needs the Writer role.
* **Security fix in this release.** Small-group suppression counts result rows, and a cross join (`MATCH (a:P {age:93}), (b:Other) RETURN …, count(*)`) or `UNWIND range(1,50)` could lift a group of one person above the threshold. Count-restricted roles now get one `MATCH` with one pattern and no `WITH`/`UNWIND`/`OPTIONAL MATCH`/`MERGE`/join/`CALL` after it. When the pattern has relationships, the count must be `count(DISTINCT node)`, because a row is then a path, not a person. This affected v0.19/v0.20.
