# Regtest harness

Local, ephemeral Bitcoin Core + two Core Lightning nodes for testing cln-hub end-to-end. No real funds, no internet, no risk.

## Files

| File | Purpose |
|---|---|
| `bitcoin.conf` | Bitcoin Core regtest — RPC `127.0.0.1:18443`, P2P `127.0.0.1:18444` |
| `lightning1.conf` | CLN node 1 — LN P2P `127.0.0.1:19735`, alias `cln-hub-test-1` |
| `lightning2.conf` | CLN node 2 — LN P2P `127.0.0.1:19736`, alias `cln-hub-test-2` |

RPC auth: `cln-hub-dev` / `cln-hub-dev`. Localhost-only. Don't reuse outside this harness.

## Runtime layout

Configs are committed under `examples/regtest/`. All runtime state — chain data, wallets, CLN datadirs, logs, pidfiles — lives under `.dev/` at the repo root and is gitignored.

```
examples/regtest/        ← tracked: bitcoin.conf, lightning{1,2}.conf
.dev/                    ← untracked
├── bitcoin/             ← bitcoind -datadir
├── lightning1/          ← CLN node 1 lightning-dir
│   └── regtest/         ← (created by CLN; lightning-rpc lives here)
└── lightning2/
    └── regtest/
```

## Start

Inside `nix develop` (so `bitcoind` / `lightningd` are on `$PATH`):

```sh
# 1) Bitcoind
mkdir -p .dev/bitcoin
bitcoind \
  -datadir=$PWD/.dev/bitcoin \
  -conf=$PWD/examples/regtest/bitcoin.conf \
  -daemon

# bitcoin-cli must be pointed at the same conf so it can read
# rpcuser/rpcpassword. Without `-conf=`, cli falls back to looking
# for a cookie file — but bitcoind doesn't write one when explicit
# RPC credentials are set, so the call fails with "Could not locate
# RPC credentials".
BCLI="bitcoin-cli -datadir=$PWD/.dev/bitcoin -conf=$PWD/examples/regtest/bitcoin.conf"

$BCLI -rpcwait getblockchaininfo >/dev/null
$BCLI createwallet dev
ADDR=$($BCLI getnewaddress)
# 101 = mine 1 spendable coinbase (coinbase matures after 100 confirms).
$BCLI generatetoaddress 101 "$ADDR" >/dev/null

# 2) Lightning node 1
#
# `--bitcoin-datadir` is forwarded to bcli's internal bitcoin-cli
# subprocess as `-datadir=`. Without it, bitcoin-cli would read
# `~/.bitcoin/bitcoin.conf` and (if it sets a `datadir=` pointing
# elsewhere) fail to find the regtest datadir. Always pass it.
mkdir -p .dev/lightning1
lightningd \
  --lightning-dir=$PWD/.dev/lightning1 \
  --conf=$PWD/examples/regtest/lightning1.conf \
  --bitcoin-datadir=$PWD/.dev/bitcoin \
  --daemon

# 3) Lightning node 2
mkdir -p .dev/lightning2
lightningd \
  --lightning-dir=$PWD/.dev/lightning2 \
  --conf=$PWD/examples/regtest/lightning2.conf \
  --bitcoin-datadir=$PWD/.dev/bitcoin \
  --daemon
```

CLI shorthands:

```sh
alias bcli="bitcoin-cli -datadir=$PWD/.dev/bitcoin -conf=$PWD/examples/regtest/bitcoin.conf"
# `lightning-cli` defaults to network=bitcoin and would look for the
# RPC socket under `<lightning-dir>/bitcoin/lightning-rpc`. Our nodes
# are regtest, so we tell the cli explicitly (otherwise: "Moving
# into '.../bitcoin': No such file or directory").
alias l1="lightning-cli --lightning-dir=$PWD/.dev/lightning1 --network=regtest"
alias l2="lightning-cli --lightning-dir=$PWD/.dev/lightning2 --network=regtest"

l1 getinfo
l2 getinfo
```

## Fund a node + open a channel

```sh
# Fund node 1 with 1 BTC on-chain.
L1_ADDR=$(l1 newaddr | jq -r .bech32)
bcli sendtoaddress "$L1_ADDR" 1
bcli generatetoaddress 1 "$ADDR"  # confirm

# Connect node 1 → node 2.
L2_ID=$(l2 getinfo | jq -r .id)
l1 connect "$L2_ID@127.0.0.1:19736"

# Open a 1M-sat channel from node 1.
l1 fundchannel "$L2_ID" 1000000
bcli generatetoaddress 1 "$ADDR"  # confirm (funding-confirms=1 in our config)

l1 listpeerchannels
```

## Load cln-hub against node 1

Either:

A) Append to `lightning1.conf`:
```
plugin=/abs/path/to/cln-hub/target/debug/cln-hub
cln-hub-bind=127.0.0.1:3000
```
and restart node 1.

B) Hot-load via CLI:
```sh
cargo build
l1 plugin start $PWD/target/debug/cln-hub
```

Test:
```sh
curl -s http://127.0.0.1:3000/getinfo | jq
```

## Stop

```sh
l1 stop
l2 stop
bcli stop
```

(With the aliases above. Without them, `bcli` becomes `bitcoin-cli -datadir=$PWD/.dev/bitcoin -conf=$PWD/examples/regtest/bitcoin.conf`.)

## Reset (wipe chain + node state)

```sh
rm -rf .dev/
```
