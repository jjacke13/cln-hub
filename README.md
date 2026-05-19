# cln-hub

> **HAS BEEN TESTED ON MAINNET USING ZEUS MOBILE WALLET**
>
> **USE WITH CAUTION IN MAINNET AND DON'T EXPOSE IT PUBLICLY WITHOUT ANY HTTPS REVERSE PROXY**

cln-hub is a [Core Lightning](https://github.com/ElementsProject/lightning) plugin that turns your CLN node into a multi-tenant custodial Lightning service.

It exposes the same [LndHub](https://github.com/BlueWallet/LndHub) HTTP API that wallet apps like [BlueWallet](https://bluewallet.io) and [Zeus](https://zeusln.com) already speak. Your friends, family, or community can scan a QR code, connect their wallet, and use your node — without each of them needing their own Lightning channels, on-chain wallet, or backups. You hold the funds; they hold a login.

Shipped as a single Rust binary you drop next to `lightningd`.

## Build with Nix (recommended)

```sh
nix develop          # drops you into a Rust + clightning shell
cargo build          # debug build at target/debug/cln-hub
```

For a stripped release binary:

```sh
nix build .#cln-hub  # → ./result/bin/cln-hub
```

The Nix flake pins every dependency for reproducible builds across machines.

## Build on Ubuntu without Nix

Tested on Ubuntu 24.04 LTS. Should work on any Debian-based distro with a recent Rust toolchain.

```sh
# 1. System dependencies.
sudo apt update
sudo apt install -y build-essential curl pkg-config

# 2. Rust toolchain (skip if you already have rustup).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 3. Build cln-hub.
git clone https://github.com/jjacke13/cln-hub.git
cd cln-hub
cargo build --release
# binary at target/release/cln-hub
```

Core Lightning itself is a separate install — see [the official CLN install guide](https://docs.corelightning.org/docs/installation) for your distro.

## Plug it into lightningd

Add to `~/.lightning/config`:

```
plugin=/path/to/cln-hub
cln-hub-bind=127.0.0.1:3000
```

Restart `lightningd`. Look for these log lines:

```
plugin-cln-hub: cln-hub plugin started
plugin-cln-hub: cln-hub HTTP listening on 127.0.0.1:3000
```

Create an account:

```sh
curl -s -X POST http://127.0.0.1:3000/create -d '{}' \
  -H 'Content-Type: application/json' | jq
```

Use the returned `login` and `password` with any LndHub-speaking wallet — Zeus, BlueWallet, or one of the many others.

## License

MIT.
