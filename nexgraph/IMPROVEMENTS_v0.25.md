# nexgraph v0.25

Fixes from running v0.24 on the ten crypto-compliance datasets (about 343k rows, 508k nodes once loaded). See
`CRYPTO_COMPLIANCE_TEST_REPORT.md` for the measurements.

| Area | Before | Now |
|---|---|---|
| Memory during `checkpoint()` | op list of every entity cloned, then encoded (second copy), sealed, framed (more copies), and the new store built while the old one was alive: peak 4.4 GB for 508k nodes | payload is encoded straight from the store one entity at a time; the old store is dropped before the new one is built; the frame is written without a copy. Same load: 1.8 GB peak. Checkpoint files are byte-identical to v0.24. |
| Memory when opening a directory database | file bytes + decrypted copy + decoded ops + cloned store alive together: 4.0 GB for 742k wide rows | buffers dropped before applying; ops are moved (`apply_owned`) not cloned: 2.1 GB, and 3.0 s instead of 4.9 s |
| Oversized checkpoint | the frame length is a u32, so a payload of 4 GiB or more would wrap and write an unreadable file | refused with a clear error before anything is written |
| `profile` on crypto data | any column whose name contains "address" was a High-risk street address: 7 of 10 such columns in these files were counts or 0/1 flags | street-address rule needs non-numeric content; new `Kind::WalletAddress` for Ethereum and Bitcoin address columns (pseudonymised with a stable keyed pseudonym by `suggest_config`, so joins survive; masked as `pseudonym` by `suggest_policy_json`) |
| Keyed `import_csv_nodes` on a non-unique key | rows silently merged (32,697 of 185,605 heist rows) with only `nodes_updated` to hint at it | new `ImportReport::duplicate_keys` counts rows whose key already appeared earlier in the same file |

## Behaviour changes
* `profile::Kind` has a new variant `WalletAddress`; code matching `Kind` exhaustively needs one more arm.
* `ImportReport` has a new field `duplicate_keys` (it derives `Default`; struct literals need `..Default::default()`).

## Not changed (measured, see the report)
Resident size is still about 2.9 KB per node with 14 properties (each node owns a `BTreeMap<String, Value>`), so a 2.97M-row
wide table needs roughly 8.6 GB to hold and about 12 GB to import. Interning property keys is the next real step and
touches the public `Props` type, so it is not part of this release. `import_csv_nodes` is still one atomic transaction.

## Tests
`tests/v025.rs` (3 tests); full suite 289 passed, 0 failed (release). `bench/crypto_compliance/` holds the harness.
HTTPS server, PostgreSQL paths and the WASM frontend build were not exercised in this environment.
