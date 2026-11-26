# nexgraph web UI (Yew + Stylist)

A single-page WebAssembly app for nexgraph, built on the Enterprise Suite v6 UI design flow:
sign-in → dashboard → role-aware sidebar → list/detail pages → generic result renderer → administration, with the
same design tokens, light / dark / system theme, shell, tables, charts and form primitives.

| Page | What it does | API |
|---|---|---|
| Sign in | tenant (empty for platform admins), user, password | `POST /v1/login`, `GET /v1/me` |
| Dashboard | engine counters, nodes/edges by label, recent audit, tenant summary | `/v1/stats`, `/v1/bi/tables`, `/v1/audit`, `/v1/admin/tenants` |
| Query workbench | NQL editor, `$param` JSON, history, sortable/paged results, CSV | `POST /v1/query` |
| Analytics | launcher for 22 `CALL` procedures (graph, spectral, diffusion, GAAP) with typed parameters; result chart + table; "open in workbench" | `POST /v1/query` |
| Data explorer | BI tables with `AS OF` time travel, search, sort, CSV | `/v1/bi/tables[/{name}]` |
| Power BI | generated Power Query (M) script and connection steps | `/v1/bi/powerquery` |
| Audit log | newest-first, tenant filter for platform admins | `/v1/audit` |
| Tenants / Users | create, disable/enable, delete tenant (type-to-confirm), reset passwords | `/v1/admin/*` |
| My account | change password, checkpoint (tenant_admin), theme | `/v1/me/password`, `/v1/checkpoint` |

Navigation mirrors the server's role policy (`reader < writer < tenant_admin < platform_admin`; platform admins have no
tenant graph, so they only see administration). The server still enforces everything.

## Build and run

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk                      # fetches a matching wasm-bindgen-cli

cd frontend && trunk build --release     # -> frontend/dist

# production: the API server serves the UI from the same origin
nexgraph-server serve --pg URL --cert cert.pem --key key.pem --frontend frontend/dist
#   (or NEXGRAPH_FRONTEND_DIR=frontend/dist)

# development: live reload, /v1 proxied to a local plain-HTTP server
nexgraph-server serve --pg URL --insecure-http --listen 127.0.0.1:8443 &
cd frontend && trunk serve --open
```

Without `--frontend` the server behaves exactly as before (every non-API path is 404). With it, `GET`/`HEAD` paths
outside `/v1/` are served from the directory, extension-less paths fall back to `index.html`, and `index.html` gets a
Content-Security-Policy that allows only same-origin scripts/connections, WebAssembly, and the SHA-256 of trunk's
inline loader. All other responses keep `default-src 'none'`.

## Tests

`cd frontend && cargo test` runs the pure logic natively (formatting, role mirror, parameter/JSON handling, history,
chart selection, navigation per role, launcher statement building). `cargo test --test ui_catalog` (repo root) runs every
launcher entry against the real engine, and `cargo test --lib --features server server::tests` covers static serving
(traversal, SPA fallback, CSP hash). The WebAssembly build itself is exercised by `trunk build`.
