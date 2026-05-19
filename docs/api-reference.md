# cln-hub API reference

cln-hub speaks the [LndHub](https://github.com/BlueWallet/LndHub) HTTP REST API. Any wallet that talks to a vanilla LndHub server (BlueWallet, Zeus, …) works against cln-hub without modification.

This document is the contract. For setup and build instructions see the [README](../README.md).

## Conventions

- Base URL: `http://<host>:<port>` (default `127.0.0.1:3000`). Put a TLS-terminating reverse proxy in front for any non-LAN exposure.
- All requests and responses use JSON.
- Authenticated endpoints require `Authorization: Bearer <access_token>`.
- Success responses are documented per-endpoint below.
- Errors are returned with an HTTP status code AND a JSON envelope:

```json
{
  "error": true,
  "code": 7,
  "message": "invoice already paid"
}
```

## Public endpoints

No authentication required.

### `GET /` and `GET /version`

Manifest blob — useful for liveness probes and version checks.

**Response 200:**

```json
{
  "name": "cln-hub",
  "version": "0.1.0",
  "node": "core-lightning"
}
```

### `GET /getinfo`

Passthrough to `lightningd getinfo`. Returns whatever CLN's `getinfo` returns. Use it to verify connectivity to the underlying CLN node.

### `POST /create`

Mint a new custodial account. Rate-limited per IP.

**Request body (any of these works):**

```json
{}
```

The body is ignored; cln-hub accepts the `{partnerid, accounttype}` shape some clients send.

**Response 200:**

```json
{
  "login": "e21de55ac8643423b58c",
  "password": "bd76dd6a2e494b2803de"
}
```

Both fields are 20 hex characters (10 random bytes). The server never logs or returns these again — save them.

### `POST /auth`

Exchange credentials OR a refresh token for a new access/refresh pair. Rate-limited per IP.

**Request body — credential mode:**

```json
{
  "login": "e21de55ac8643423b58c",
  "password": "bd76dd6a2e494b2803de"
}
```

**Request body — refresh mode:**

```json
{
  "refresh_token": "dec467d79cd367a9f53e2fc9ac26fa9108a3ce48"
}
```

**Response 200:**

```json
{
  "access_token":  "33f07a3ee254ac620f9c36aa06ec667c1b9405f7",
  "refresh_token": "dec467d79cd367a9f53e2fc9ac26fa9108a3ce48"
}
```

`access_token` is valid for 7 days, `refresh_token` for 31 days. Send the access token in the `Authorization: Bearer …` header on every authenticated request.

### `POST /decodeinvoice`

Decode a BOLT11 invoice string by asking CLN. Returns CLN's decoded body plus a few LndHub-flavoured aliases (`destination`, `num_satoshis`, `num_msat`, `timestamp`, `expire_time`).

**Request body:**

```json
{ "invoice": "lnbc..." }
```

**Response 200:** the decoded invoice as a JSON object.

The `invoice` string is capped at 4 KB.

## Authenticated endpoints

Send `Authorization: Bearer <access_token>` with every request. A missing, malformed, expired, or forged token returns HTTP 401.

### `POST /addinvoice`

Issue a BOLT11 invoice owned by the calling user. When this invoice is paid (by anyone, externally or via the hub's internal short-circuit), the amount is credited to the caller's ledger.

**Request body:**

```json
{ "amt": 5000, "memo": "coffee" }
```

`amt` is in **satoshis**, may be a number or a string of digits.

**Response 200:**

```json
{
  "r_hash": "1463978eaad6d7615278c4fefc18e9cd8f63eecfebcf420fcd6543ea9891ddca",
  "payment_request": "lnbc70u1p4qkh88sp52y…",
  "pay_req":         "lnbc70u1p4qkh88sp52y…",
  "add_index":       ""
}
```

### `GET /getuserinvoices`

List the calling user's invoices, newest first.

**Response 200:** an array of invoice objects. Each object has the shape:

```json
{
  "r_hash":           "1463978eaad6d761…",
  "payment_request":  "lnbc70u1p4qkh88sp52y…",
  "ispaid":           false,
  "type":             "user_invoice",
  "amt":              7000,
  "amt_msat":         7000000,
  "settled_amt_msat": null,
  "expire_time":      3600,
  "timestamp":        1747688400,
  "description":      "coffee"
}
```

`ispaid` flips to `true` once the invoice settles. `settled_amt_msat` is `null` until then; afterwards it holds the actual paid amount in millisatoshis. `expire_time` is the remaining validity window in seconds at issue time (CLN-supplied `expires_at` minus our creation timestamp). `timestamp` is unix epoch seconds.

### `POST /payinvoice`

Pay a BOLT11 invoice from the calling user's balance.

cln-hub picks one of two paths automatically:

- **Internal short-circuit** — if the invoice was issued by another user on this same hub, settlement happens atomically inside cln-hub. No Lightning HTLC, no fee.
- **External pay** — otherwise, cln-hub reserves the amount plus a small fee buffer, asks CLN to pay over Lightning, then settles or refunds depending on the outcome.

**Request body:**

```json
{ "invoice": "lnbc...", "amount": 5000 }
```

`amount` (in satoshis) is required only for amountless BOLT11s and ignored otherwise.

**Response 200 — success:**

```json
{
  "payment_error":    "",
  "payment_preimage": "bd3b2dc9b4acee0a7fffb8f50518fab5bd4a85348aae17fa93b36c83fee3bc24",
  "payment_route": {
    "total_amt":       5000,
    "total_amt_msat":  5000000,
    "total_fees":      0,
    "total_fees_msat": 0
  },
  "decoded": {
    "destination":  "0297dcbac5a213fb59d…",
    "payment_hash": "8d994ba2c9c2aa5aea…",
    "num_satoshis": "5000",
    "num_msat":     "5000000",
    "description":  "coffee"
  }
}
```

For internal payments the `payment_preimage` is all zeros (no real preimage exists when the payment never touches the Lightning network).

**Error responses:**

| HTTP | code | Meaning |
|---|---|---|
| 400 | 0 | malformed input |
| 402 | 5 | insufficient balance |
| 402 | 6 | external payment failed or in-flight |
| 402 | 7 | bad invoice / already paid / decode failure / oversize string |

The `invoice` string is capped at 4 KB.

### `GET /getbalance` and `GET /balance`

Return the calling user's current balance.

**Response 200:**

```json
{
  "BTC": {
    "AvailableBalance":     1041000,
    "AvailableBalanceMsat": 1041000000
  }
}
```

`/balance` is an older LndHub alias for the same endpoint; both return the same body.

### `GET /gettxs`

List the calling user's outbound Lightning payments and confirmed on-chain deposits, newest first. The response is a single array; entries are discriminated by their `type` field.

**`type: "paid_invoice"`** — a completed outbound Lightning payment:

```json
{
  "type":             "paid_invoice",
  "value":            4000,
  "value_msat":       4000000,
  "fee":              0,
  "fee_msat":         0,
  "memo":             "coffee",
  "payment_hash":     "2f4f11f4a959a82bfbc15d4c1dff6…",
  "payment_preimage": "54dac17feb9bf90d9432d533d616…",
  "payment_request":  "lnbcrt40u1p4qkhngsp5wakatef…",
  "status":           "external_settled",
  "timestamp":        1779129960,
  "settled_at":       1779129960
}
```

`status` is either `internal` (hub-to-hub short-circuit, no Lightning HTLC) or `external_settled` (paid over Lightning). Internal payments report `payment_preimage` as 64 zeros — no real preimage exists when the payment never touches the network.

**`type: "bitcoind_tx"`** — a confirmed on-chain deposit to the caller's `/getbtc` address, in LndHub's canonical shape:

```json
{
  "type":         "bitcoind_tx",
  "amount":       0.0007,
  "amount_msat":  70000000,
  "category":     "receive",
  "confirmations": null,
  "address":      "bc1q2qln3x922t0ejanj3xny8uu9ygg84ct9v3nafu",
  "txid":         "13c134bb2fcbfd81f269fc45005a3223cd59ca4baf0ad98c137987bf41a902bc",
  "vout":         0,
  "blockheight":  112,
  "time":         1779127146,
  "timestamp":    1779127146
}
```

`amount` is a BTC-denominated float (LndHub canon); `amount_msat` is the same value in millisatoshis. `confirmations` is always `null` (we don't recompute live depth; the credit only lands after the configured `cln-hub-min-deposit-confs` threshold has been crossed).

Only **completed** payments appear here. Failed and in-flight Lightning payments are not exposed via `/gettxs`.

### `GET /getpending`

Pending on-chain deposits. Always returns `[]` — deposits credit only after they reach the configured number of confirmations (see `cln-hub-min-deposit-confs`), so there is no "pending" state visible to clients.

### `GET /getbtc`

Return the calling user's persistent on-chain deposit address. First call mints a fresh address via CLN's `newaddr`; later calls return the same address.

**Response 200:**

```json
[ { "address": "bc1q..." } ]
```

### `GET /checkpayment/:hash`

Did the **calling user's** invoice with `payment_hash = :hash` settle yet?

`:hash` must be 64 lowercase-hex characters.

**Response 200:**

```json
{ "paid": true }
```

A hash that doesn't belong to the calling user returns `{"paid": false}` — indistinguishable from "not paid yet".

## Error codes

LndHub-compatible:

| HTTP | LndHub code | When |
|---|---|---|
| 400 | 0 | Malformed input |
| 401 | 1 | Bad credentials, missing/expired/forged token |
| 402 | 5 | Insufficient balance |
| 402 | 6 | External Lightning payment unavailable, failed, or in-flight |
| 402 | 7 | Bad invoice / already paid / decode failure |
| 413 | — | Request body exceeded the size limit |
| 429 | 9 | Rate limit exceeded |
| 500 | 0 | Internal server error |

## Rate limiting

The unauthenticated `POST /create` and `POST /auth` endpoints are throttled per source IP. Clients that exceed the limit get HTTP 429 with LndHub code 9.

All other endpoints are unthrottled at the application layer; put a reverse proxy in front for global rate control if you expose the API publicly.
