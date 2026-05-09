# cln-hub

> **THIS IS A WORK IN PROGRESS**

An [LndHub](https://github.com/BlueWallet/LndHub)-compatible HTTP API server, implemented as a [Core Lightning](https://github.com/ElementsProject/lightning) plugin.

The same REST surface that BlueWallet, Zeus, and other LndHub clients already speak — but backed by a CLN node instead of LND, shipped as a single Rust binary you drop next to `lightningd`.

## Status

Pre-alpha but functional. Verified against Zeus on plain HTTP. The internal-payment ledger and token machinery have end-to-end tests; the external Lightning payment path is stubbed pending a node with channels (see [Roadmap](#roadmap)).

## Features

- **LndHub-compatible REST API** — drop-in replacement for the BlueWallet LndHub server. See [API reference](#api-reference) below.
- **Custodial accounts** — random `login`/`password` per `/create`, opaque `access_token`/`refresh_token` pair per `/auth`. Argon2id password hashing.
- **Internal payments** — when a hub user pays another hub user's invoice, the whole settlement is one atomic SQLite transaction. No fee, no Lightning traffic.
- **External payments (planned)** — wired to the same `/payinvoice` endpoint; currently returns `code 6 "external payments are not yet wired"` until channels exist.
- **On-chain deposits** — `/getbtc` mints a fresh CLN bech32 address per user; a background watcher polls `listfunds` and credits the user's ledger when deposits confirm.
- **Token TTL + cleanup** — access tokens expire after 7 days, refresh tokens after 31 days. An hourly background task prunes expired rows.
- **Per-IP rate limiting** — token-bucket on `/create` and `/auth`. No new crates; ~30-line implementation.
- **Single Nix-built binary** — ~5.8 MB, statically linked SQLite, glibc-only.

## Quick start

### Prerequisites

A running Core Lightning node. Mainnet works for everything except external payments (`/payinvoice` to invoices outside this hub) — that needs channels.

### Build

```sh
nix develop          # drops you into a Rust + clightning shell
cargo build          # debug build at target/debug/cln-hub
```

Or for a stripped release binary:

```sh
nix build .#cln-hub  # → ./result/bin/cln-hub
```

### Configure lightningd

Add to `~/.lightning/config`:

```
plugin=/path/to/cln-hub
cln-hub-bind=127.0.0.1:3000
# cln-hub-db=/var/lib/cln-hub/cln-hub.db   # default: <lightning-dir>/cln-hub.db
```

Restart `lightningd`. Verify:

```sh
grep cln-hub ~/.lightning/lightning.log
# plugin-cln-hub: cln-hub plugin started
# plugin-cln-hub: cln-hub HTTP listening on 127.0.0.1:3000
```

### Smoke test

```sh
# Create an account.
curl -s -X POST http://127.0.0.1:3000/create -d '{}' \
  -H 'Content-Type: application/json' | jq

# Use the returned login + password to /auth, then hit /getinfo or /getbalance.
```

### Plugin options

| Option | Default | Description |
|---|---|---|
| `cln-hub-bind` | `127.0.0.1:3000` | host:port for the HTTP API |
| `cln-hub-db`   | `<lightning_dir>/cln-hub.db` | SQLite database path |

## Architecture

```
   wallet apps (BlueWallet / Zeus / ...)
              │ HTTP, LndHub REST
              ▼
   ┌─────────────────────────────────────────┐
   │  cln-hub  (single binary)               │
   │  ─────────────────────────────────────  │
   │   axum HTTP server   ← rate-limit       │
   │      │               ← AuthUser         │
   │      ▼                                  │
   │   handlers                              │
   │      ├─ ledger/users/invoices via sqlx  │
   │      └─ JSON-RPC into lightningd        │
   │                                         │
   │   background tasks:                     │
   │      ├─ invoice_payment subscription    │
   │      ├─ on-chain deposit watcher        │
   │      └─ token cleanup                   │
   └─────────────────────────────────────────┘
              ▲ JSON-RPC (stdin/stdout)
              │
   ┌─────────────────────────────────────────┐
   │             lightningd                  │
   └─────────────────────────────────────────┘
```

### Module layout

```
src/
├── main.rs       — plugin lifecycle, spawns HTTP + background tasks
├── cln.rs        — JSON-RPC client over the lightning-rpc unix socket
├── db.rs         — SQLite pool, migrations, per-table CRUD, atomic txs
├── plugin.rs     — invoice_payment subscription, deposit watcher
├── state.rs      — AppState (rpc_path + DB pool), shared via Arc
└── http/
    ├── mod.rs    — Router, AppError, AuthUser, middleware
    ├── auth.rs   — /create, /auth
    ├── info.rs   — /decodeinvoice, /checkpayment/:hash
    ├── invoice.rs— /addinvoice, /getuserinvoices
    ├── payment.rs— /payinvoice, /getbalance, /gettxs, /getpending, /getbtc
    └── ratelimit.rs — token-bucket rate limiter
```

### Schema (SQLite)

| Table | Rows | Purpose |
|---|---|---|
| `users` | one per account | `login` (plaintext, unique) + `password_hash` (argon2id) |
| `tokens` | one per `/auth` mint | `access_token` + `refresh_token` + `created_at` (TTL filter at lookup) |
| `invoices` | one per `/addinvoice` | maps our `label` → owning user; `settled_at` flips on payment |
| `payments` | one per `/payinvoice` | outbound payment record (status: `internal` / `external_*`) |
| `ledger` | append-only | `kind`, `amount_msat` (signed); `SUM` = balance |
| `addresses` | one per user | persistent on-chain deposit address |
| `onchain_credits` | one per credited UTXO | idempotency key `(txid, vout)` for the deposit watcher |

## API reference

All endpoints accept JSON. Errors return `{"error": true, "code": <int>, "message": "..."}`.

### Public

| Method | Path | Purpose | Notes |
|---|---|---|---|
| GET | `/` | Manifest | `{name, version, node}` |
| GET | `/version` | Same as `/` | |
| GET | `/getinfo` | CLN node info | passthrough to `lightningd getinfo` |
| POST | `/create` | New account | empty body OK; returns `{login, password}`; **rate-limited** |
| POST | `/auth` | Token mint | `{login, password}` OR `{refresh_token}`; **rate-limited** |
| POST | `/decodeinvoice` | Decode BOLT11 | `{invoice}` → CLN-decoded + LndHub-aliased fields |
| GET | `/checkpayment/:hash` | Settle status | `{paid: bool}` for an invoice we issued |

### Authenticated (`Authorization: Bearer <access_token>`)

| Method | Path | Purpose | Request | Response |
|---|---|---|---|---|
| POST | `/addinvoice` | Mint a BOLT11 | `{amt: <sats>, memo}` (amt may be string or number) | `{r_hash, payment_request, pay_req, add_index}` |
| GET | `/getuserinvoices` | Inbound history | — | array of invoices, newest first |
| POST | `/payinvoice` | Pay BOLT11 | `{invoice, amount?: <sats>}` | LndHub success shape (internal); `code 6` on external (planned) |
| GET | `/getbalance` | Current balance | — | `{"BTC": {"AvailableBalance": <sats>, "AvailableBalanceMsat": <msat>}}` |
| GET | `/balance` | Alias of `/getbalance` | — | same |
| GET | `/gettxs` | Outbound history | — | array of payments, newest first |
| GET | `/getpending` | Pending on-chain | — | always `[]` (deposits credit immediately on confirmation) |
| GET | `/getbtc` | On-chain deposit address | — | `[{address}]` (persistent per user) |

### Error codes (LndHub-compatible)

| HTTP | LndHub code | When |
|---|---|---|
| 400 | 0 | Malformed input |
| 401 | 1 | Bad creds, missing/expired/forged token |
| 402 | 5 | Insufficient balance |
| 402 | 6 | External payment unavailable |
| 402 | 7 | Bad invoice / already paid / decode failure |
| 429 | 9 | Rate limit exceeded |
| 500 | 0 | Internal server error |

## Roadmap

| Slice | Status | Description |
|---|---|---|
| 1 | ✅ | Plugin scaffold; lightningd loads us cleanly |
| 2 | ✅ | HTTP server (axum) + `/getinfo` passthrough |
| 3 | ✅ | SQLite users + `/create` + `/auth` (argon2id, opaque tokens) |
| 4 | ✅ | `/addinvoice` + `invoice_payment` notification → atomic credit |
| 5a | ✅ | `/payinvoice` internal short-circuit, `/getbalance`, `/gettxs`, `/decodeinvoice`, `/checkpayment` |
| 5b | ⏳ | External `/payinvoice` (CLN `pay`) — needs channels |
| 5c | ✅ | Token TTL + per-IP rate limiting |
| 5d | ✅ | Hourly token cleanup + `/getbtc` |
| 5e | ✅ | On-chain deposit watcher (`listfunds` → `ledger`) |
| — | 📋 | NixOS module (`services.cln-hub.enable = true`) |
| — | 📋 | TLS / reverse-proxy story for non-LAN exposure |
| — | 📋 | Real wallet client compat polish (Buffer-style `r_hash`, etc.) |

## Known limitations

- **No HTTPS.** Bind to localhost; for non-LAN exposure put Caddy/nginx in front.
- **External Lightning payments (`/payinvoice` to non-hub invoices) return `code 6`.** Slice 5b will wire CLN's `pay` once we have channels to test against.
- **Token rotation is additive.** Old tokens stay valid until TTL expires (matches original LndHub). No revoke-on-rotate.
- **Rate-limit IP tracking is in-memory.** Restart resets state; map grows with distinct IPs ever seen (fine for LAN, add a pruner before public exposure).
- **No per-login backoff** on `/auth` brute-force, only per-IP. Attacker rotating IPs (Tor / proxy pool) bypasses.

## Testing manually

The project has no automated test suite yet (TODO). Manual end-to-end test commands live in the slice notes — see git log for `feat:` commits, each ships with a verification recipe.

## License

MIT.
