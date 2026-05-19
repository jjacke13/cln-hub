# cln-hub

> **DO NOT USE WITH REAL FUNDS YET**
>
> **THIS IS A WORK IN PROGRESS**

An [LndHub](https://github.com/BlueWallet/LndHub)-compatible HTTP API server, implemented as a [Core Lightning](https://github.com/ElementsProject/lightning) plugin.

The same REST surface that BlueWallet, Zeus, and other LndHub clients already speak — but backed by a CLN node instead of LND, shipped as a single Rust binary you drop next to `lightningd`.

## Status

Pre-alpha but functional. Verified against Zeus on plain HTTP; a unit test suite covers every atomic database path and the regtest harness exercises the full external Lightning pay + on-chain deposit flows.

## Features

- **Full LndHub-compatible REST API** — drop-in replacement for the BlueWallet LndHub server. See [API reference](#api-reference) below.
- **Custodial accounts** — random `login`/`password` per `/create`, opaque `access_token`/`refresh_token` pair per `/auth`. Argon2id password hashing.
- **Internal payments** — when a hub user pays another hub user's invoice, the whole settlement is one atomic SQLite transaction. No fee, no Lightning traffic.
- **External Lightning payments** — `/payinvoice` reserves balance + fee, calls CLN `pay`, then settles or refunds atomically. Distinguishes terminal failure from in-flight CLN error codes. A background reconciler resolves crash-mid-pay state via `listpays`.
- **On-chain deposits** — `/getbtc` mints a fresh CLN bech32 address per user; a background watcher polls `listfunds` and credits the user's ledger when deposits reach the configured number of confirmations.
- **Token TTL + cleanup** — access tokens expire after 7 days, refresh tokens after 31 days. An hourly background task prunes expired rows.
- **Per-IP rate limiting** — token-bucket on `/create` and `/auth`. Hand-rolled, no new crates.
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
# cln-hub-min-deposit-confs=6              # default: 6
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
| `cln-hub-bind`              | `127.0.0.1:3000`               | host:port for the HTTP API |
| `cln-hub-db`                | `<lightning_dir>/cln-hub.db`   | SQLite database path |
| `cln-hub-min-deposit-confs` | `6`                            | min confirmations before a `/getbtc` deposit credits the user's ledger |

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
   │      ├─ external-pay reconciler         │
   │      └─ hourly token cleanup            │
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
├── plugin.rs     — invoice_payment subscription, deposit watcher, reconciler
├── state.rs      — AppState (rpc_path + DB pool + min_deposit_confs), shared via Arc
└── http/
    ├── mod.rs    — Router, AppError, AuthUser, middleware
    ├── auth.rs   — /create, /auth
    ├── info.rs   — /decodeinvoice, /checkpayment
    ├── invoice.rs— /addinvoice, /getuserinvoices
    ├── payment.rs— /payinvoice, /getbalance, /balance, /gettxs, /getpending, /getbtc
    └── ratelimit.rs — token-bucket rate limiter
```

### Schema (SQLite)

| Table | Rows | Purpose |
|---|---|---|
| `users` | one per account | `login` (plaintext, unique) + `password_hash` (argon2id) |
| `tokens` | one per `/auth` mint | access + refresh tokens + `created_at` (TTL filter at lookup) |
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
| GET  | `/`              | Manifest | `{name, version, node}` |
| GET  | `/version`       | Same as `/` | |
| GET  | `/getinfo`       | CLN node info | passthrough to `lightningd getinfo` |
| POST | `/create`        | New account | empty body OK; returns `{login, password}`; **rate-limited** |
| POST | `/auth`          | Token mint | `{login, password}` OR `{refresh_token}`; **rate-limited** |
| POST | `/decodeinvoice` | Decode BOLT11 | `{invoice}` → CLN-decoded + LndHub-aliased fields |

### Authenticated (`Authorization: Bearer <access_token>`)

| Method | Path | Purpose | Request | Response |
|---|---|---|---|---|
| POST | `/addinvoice`         | Mint a BOLT11             | `{amt: <sats>, memo}` (amt may be string or number) | `{r_hash, payment_request, pay_req, add_index}` |
| GET  | `/getuserinvoices`    | Inbound history           | — | array of invoices, newest first |
| POST | `/payinvoice`         | Pay BOLT11 (internal short-circuit OR external via CLN `pay`) | `{invoice, amount?: <sats>}` | LndHub success shape with real preimage on external |
| GET  | `/getbalance`         | Current balance           | — | `{"BTC": {"AvailableBalance": <sats>, "AvailableBalanceMsat": <msat>}}` |
| GET  | `/balance`            | Alias of `/getbalance`    | — | same |
| GET  | `/gettxs`             | Outbound history + confirmed on-chain credits | — | array, newest first |
| GET  | `/getpending`         | Pending on-chain          | — | always `[]` (deposits credit only after `min_deposit_confs`) |
| GET  | `/getbtc`             | On-chain deposit address  | — | `[{address}]` (persistent per user) |
| GET  | `/checkpayment/:hash` | Did the caller's invoice with this hash settle yet? | — | `{paid: bool}` |

### Error codes (LndHub-compatible)

| HTTP | LndHub code | When |
|---|---|---|
| 400 | 0 | Malformed input |
| 401 | 1 | Bad creds, missing/expired/forged token |
| 402 | 5 | Insufficient balance |
| 402 | 6 | External payment unavailable / failed / in-flight |
| 402 | 7 | Bad invoice / already paid / decode failure |
| 429 | 9 | Rate limit exceeded |
| 500 | 0 | Internal server error |

## Roadmap

| Slice | Status | Description |
|---|---|---|
| 1 | ✅ | Plugin scaffold; lightningd loads us cleanly |
| 2 | ✅ | HTTP server (axum) + `/getinfo` passthrough |
| 3 | ✅ | SQLite users + `/create` + `/auth` |
| 4 | ✅ | `/addinvoice` + `invoice_payment` notification → atomic credit |
| 5a | ✅ | `/payinvoice` internal short-circuit, `/getbalance`, `/gettxs`, `/decodeinvoice`, `/checkpayment` |
| 5b | ✅ | External `/payinvoice` (CLN `pay`) — reserve/refund + reconciler |
| 5c | ✅ | Token TTL + per-IP rate limiting |
| 5d | ✅ | Hourly token cleanup + `/getbtc` |
| 5e | ✅ | On-chain deposit watcher (`listfunds` → `ledger`) |
| 6  | ✅ | Regtest harness (`examples/regtest/`) — bitcoind + 2 CLN nodes |
| —  | ✅ | `cln-hub-min-deposit-confs` plugin option |
| —  | ✅ | `/gettxs` surfaces on-chain credits alongside Lightning payments |
| —  | ✅ | GitHub Actions CI (manual trigger, x86_64 + aarch64) |
| —  | 📋 | TLS / reverse-proxy story for non-LAN exposure |
| —  | 📋 | Operator docs (channel funding, capacity rules, reconciliation playbook) |

## Known limitations

- **No HTTPS.** Bind to localhost; for non-LAN exposure put Caddy/nginx in front.
- **External payment liquidity** depends on the operator's CLN channels. Operators must keep outbound capacity ≥ sum of user balances to avoid stranded credit.
- **Token rotation is additive.** Old tokens stay valid until TTL expires (matches original LndHub).
- **Rate-limit IP tracking is in-memory.** Restart resets state; map grows with distinct IPs ever seen (fine for LAN).
- **`/gettxs` includes on-chain credits as `bitcoind_tx` entries** (LndHub canonical shape). **Zeus** mis-renders these as outgoing because Zeus's source has no handling for the `bitcoind_tx` type. BlueWallet, which inherits the original LndHub conventions, is expected to render correctly (untested as of this writing). Accounting is unaffected; `/getbalance` includes the deposit regardless.

## Testing

### Unit tests

```sh
cargo test
```

A full suite against an in-memory SQLite, covering every atomic database path: user create/verify, balance arithmetic, internal payment paths, external pay reserve/settle/fail (with concurrency guards), on-chain credit idempotency, invoice settle idempotency, and token TTL + cleanup.

### Regtest end-to-end

A complete bitcoind + 2-CLN-node harness lives under [`examples/regtest/`](examples/regtest/). Boots in ~10 seconds, opens a real channel between the two nodes, and lets you load `cln-hub` against one of them for full external-pay + on-chain deposit testing. See [examples/regtest/README.md](examples/regtest/README.md) for recipes.

### CI

Manual-trigger GitHub Actions workflow (`.github/workflows/ci.yml`) runs the build + test pipeline on both `ubuntu-24.04` (x86_64) and `ubuntu-24.04-arm` (aarch64). Trigger via GitHub UI → Actions → CI → Run workflow, or `gh workflow run ci.yml`.

## License

MIT.
