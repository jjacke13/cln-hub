// src/cln.rs
//
// Tiny JSON-RPC client for talking to `lightningd` over its unix
// socket. Used by every HTTP handler that needs to ask CLN something.
//
// Why hand-rolled, instead of using the `cln-rpc` crate?
//   - cln-rpc gives you typed wrappers around each CLN method
//     (getinfo, listinvoices, pay, ...). Nice, but its types drift
//     between releases and every CLN version adds new methods.
//   - LndHub responses are loosely-typed JSON anyway — we mostly want
//     to forward fields verbatim or pluck a couple out, so an untyped
//     `serde_json::Value` is friendlier and means one fewer dep to
//     keep on the right version.
//   - It's also short enough to be educational: the actual wire
//     protocol is ~15 lines.
//
// One connection per call. CLN's lightning-rpc supports request
// pipelining on a single connection, but the socket is local (kernel
// memory, microsecond latency) so the cost of opening a fresh socket
// per HTTP request is negligible compared to the simplicity gain.

use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

// Tokio's async I/O traits and concrete types. The `*Ext` traits
// provide the `read_line`, `write_all`, etc. methods on whatever
// implements `AsyncRead`/`AsyncWrite`.
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Make a one-shot RPC call against lightningd.
///
/// Arguments:
///   - `socket`: filesystem path to `lightning-rpc` (e.g.
///     `~/.lightning/regtest/lightning-rpc`).
///   - `method`: CLN method name (e.g. `"getinfo"`, `"invoice"`).
///   - `params`: JSON params object/array, or `json!({})` for none.
///
/// Returns: the unwrapped `result` field on success, or an `Err`
/// containing CLN's error object if the call failed.
pub async fn call(socket: &Path, method: &str, params: Value) -> Result<Value> {
    // === Rust note: `&Path` ===
    //
    // `&Path` is a borrowed reference to a filesystem path — like
    // `const std::filesystem::path&` in C++. The function doesn't take
    // ownership; it just reads the path during the call. The owned
    // counterpart is `PathBuf` (think `std::filesystem::path`).
    //
    // The `&` prefix means "reference, don't move". Rust's ownership
    // model says values have one owner at a time; passing by reference
    // lets a function look at a value without owning it.

    // Open the socket. `await` yields control back to the runtime
    // until the connect completes.
    let stream = UnixStream::connect(socket).await?;

    // Split the duplex stream into independent read & write halves.
    // Without this, the borrow checker would refuse to let us read
    // and write concurrently — a single `&mut UnixStream` can only
    // be used for one at a time.
    let (read_half, mut write_half) = stream.into_split();

    // Wrap the read half in `BufReader` so we get the `read_line`
    // helper (it buffers and reads up to the next `\n`).
    let mut reader = BufReader::new(read_half);

    // Build the JSON-RPC 2.0 request envelope.
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    // Serialize to bytes and append a newline. CLN delimits messages
    // by newline on this socket.
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;

    // Read one line of response.
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp: Value = serde_json::from_str(&line)?;

    // JSON-RPC 2.0 responses contain either `result` or `error`,
    // never both. Pluck the appropriate one.
    if let Some(result) = resp.get("result") {
        Ok(result.clone())
    } else if let Some(error) = resp.get("error") {
        // `anyhow!` is a macro that builds an `anyhow::Error` from a
        // format-string. Like `format!` but for errors.
        Err(anyhow!("lightningd error on {}: {}", method, error))
    } else {
        Err(anyhow!("malformed lightningd response: {}", resp))
    }
}

/// Parse a CLN `msat` field as it appears on the wire.
///
/// CLN's wire format for millisatoshi values has shifted between
/// releases. We accept all known shapes:
///   - integer:                `1000`
///   - "Nmsat" string:         `"1000msat"`
///   - bare numeric string:    `"1000"`
///
/// Returns `None` for anything else.
pub fn parse_msat(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        let trimmed = s.strip_suffix("msat").unwrap_or(s);
        return trimmed.parse().ok();
    }
    None
}
