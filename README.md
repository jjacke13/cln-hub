# cln-hub

An [LndHub](https://github.com/BlueWallet/LndHub)-compatible HTTP API server, implemented as a [Core Lightning](https://github.com/ElementsProject/lightning) plugin.

The same REST surface that BlueWallet, Zeus, and other LndHub clients already speak — but backed by a CLN node instead of LND, and shipped as a single plugin binary you drop next to `lightningd`.

## Status

Pre-alpha. Slice 1 only loads the plugin. See `CLAUDE.md` for the slice plan.

## Architecture

```
   wallet apps (BlueWallet / Zeus)
              │ HTTPS, LndHub REST
              ▼
   ┌─────────────────────────────┐
   │  cln-hub  (single binary)   │
   │  ─────────────────────────  │
   │   axum HTTP server          │
   │   sqlite ledger             │
   │   cln-plugin RPC client     │◀── child process of ──┐
   └─────────────────────────────┘                       │
              │  JSON-RPC (stdin/stdout)                 │
              ▼                                          │
   ┌──────────────────────────┐                          │
   │       lightningd         │ ─────────────────────────┘
   └──────────────────────────┘
```

## Build

```
nix develop          # drops you into a shell with rust + clightning
cargo build          # produces target/debug/cln-hub
```

## Test against a real lightningd

```
lightningd --network=regtest --plugin=$PWD/target/debug/cln-hub
```

Look for `plugin-cln-hub: cln-hub plugin started` in lightningd's logs.

## Project layout

```
cln-hub/
├── flake.nix           # dev shell pin (rust + clightning + sqlite)
├── Cargo.toml          # rust dependencies
├── src/
│   └── main.rs         # plugin entry point
├── README.md
└── CLAUDE.md           # project context for AI-assisted dev
```
