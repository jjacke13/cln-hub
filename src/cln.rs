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
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

// Tokio's async I/O traits and concrete types. The `*Ext` traits
// provide the `read_line`, `write_all`, etc. methods on whatever
// implements `AsyncRead`/`AsyncWrite`.
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
// CLN normally retries routes for 60 seconds; allow headroom for a terminal result.
const PAY_RPC_TIMEOUT: Duration = Duration::from_secs(120);

/// Two error shapes from `call_strict`:
///
///   - `Transport` — we couldn't talk to lightningd (socket gone,
///     read/write/parse failed). The remote state is unknown.
///   - `Rpc` — lightningd answered with a JSON-RPC error object.
///     `code` is its error code (e.g. CLN's `PAY_STOPPED_RETRYING`
///     = 210), `message` is the human text, `data` is whatever extra
///     payload CLN included (often empty, sometimes useful — e.g.
///     `failcode` / `attempts` for failed `pay`).
///
/// Callers that need to branch on the error code (the external
/// `/payinvoice` path, slice 5b) use `call_strict`. Callers that
/// don't care use the thinner `call` below which flattens both
/// variants into `anyhow::Error`.
///
/// === Rust note: enum variants carrying different data ===
///
/// Each variant of an enum can hold its own set of fields, like a
/// tagged union. Pattern matching destructures by variant. This is
/// the idiomatic Rust replacement for "Error subclass hierarchies"
/// you'd see in OOP languages.
pub enum CallErr {
    Transport(anyhow::Error),
    Rpc {
        code: i64,
        message: String,
        #[allow(dead_code)] // present for callers who want it; not always used
        data: Value,
    },
}

impl std::fmt::Display for CallErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallErr::Transport(e) => write!(f, "transport: {:#}", e),
            CallErr::Rpc { code, message, .. } => write!(f, "lightningd rpc {}: {}", code, message),
        }
    }
}

/// Convenience: turn a CallErr into an anyhow::Error when the caller
/// doesn't need the structured fields. Used by the `call` wrapper.
impl From<CallErr> for anyhow::Error {
    fn from(e: CallErr) -> Self {
        anyhow!("{}", e)
    }
}

/// Make a one-shot RPC call against lightningd, surfacing the
/// JSON-RPC error structure when one comes back.
///
/// Arguments:
///   - `socket`: filesystem path to `lightning-rpc` (e.g.
///     `~/.lightning/regtest/lightning-rpc`).
///   - `method`: CLN method name (e.g. `"getinfo"`, `"invoice"`).
///   - `params`: JSON params object/array, or `json!({})` for none.
pub async fn call_strict(socket: &Path, method: &str, params: Value) -> Result<Value, CallErr> {
    let deadline = if method == "pay" {
        PAY_RPC_TIMEOUT
    } else {
        RPC_TIMEOUT
    };
    call_with_timeout(socket, method, params, deadline).await
}

async fn call_with_timeout(
    socket: &Path,
    method: &str,
    params: Value,
    deadline: Duration,
) -> Result<Value, CallErr> {
    // A timeout drops the unfinished future and socket; it cannot cancel an
    // accepted CLN payment. Transport errors therefore keep funds reserved.
    tokio::time::timeout(deadline, async {
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
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| CallErr::Transport(anyhow!(e)))?;

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
        let mut bytes = serde_json::to_vec(&request).map_err(|e| CallErr::Transport(anyhow!(e)))?;
        bytes.push(b'\n');
        write_half
            .write_all(&bytes)
            .await
            .map_err(|e| CallErr::Transport(anyhow!(e)))?;

        // The single deadline covers connect, write and read, rather than
        // restarting whenever a slow peer sends another byte.
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| CallErr::Transport(anyhow!(e)))?;
        let resp: Value =
            serde_json::from_str(&line).map_err(|e| CallErr::Transport(anyhow!(e)))?;

        // JSON-RPC 2.0 responses contain either `result` or `error`,
        // never both. Pluck the appropriate one.
        if let Some(result) = resp.get("result") {
            Ok(result.clone())
        } else if let Some(error) = resp.get("error") {
            // Structured error — preserve code + message for the caller.
            let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unspecified lightningd error")
                .to_string();
            let data = error.get("data").cloned().unwrap_or(Value::Null);
            Err(CallErr::Rpc {
                code,
                message,
                data,
            })
        } else {
            Err(CallErr::Transport(anyhow!(
                "malformed lightningd response: {}",
                resp
            )))
        }
    })
    .await
    .map_err(|_| {
        CallErr::Transport(anyhow!(
            "CLN {} RPC timed out after {}s; outcome is unknown",
            method,
            deadline.as_secs_f64()
        ))
    })?
}

/// Thin wrapper over `call_strict` that collapses both error variants
/// into `anyhow::Error`. Most callers don't care about the RPC code,
/// so this is the friendlier signature.
pub async fn call(socket: &Path, method: &str, params: Value) -> Result<Value> {
    call_strict(socket, method, params)
        .await
        .map_err(Into::into)
}

/// Validate a successful payment's amount before calculating its fee.
pub fn payment_fee_msat(response: &Value, amount: i64, reserve: i64) -> Result<i64> {
    let sent = parse_msat(&response["amount_sent_msat"])
        .and_then(crate::amount::from_msat)
        .ok_or_else(|| anyhow!("missing or invalid amount_sent_msat"))?;
    let fee = sent
        .checked_sub(amount)
        .filter(|n| *n >= 0 && *n <= reserve)
        .ok_or_else(|| anyhow!("CLN payment amount or fee disagrees with reservation"))?;
    Ok(fee)
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

/// Validate a CLN-returned `payment_preimage`. A preimage is the
/// 32-byte secret whose SHA-256 equals the invoice's payment_hash;
/// on the wire it's 64 lowercase hex characters. We refuse to settle
/// an external payment with an absent / empty / malformed preimage —
/// the preimage IS the off-chain receipt that lets a user prove they
/// paid, and silently storing `""` for that field destroys an audit
/// trail we may never recover.
///
/// Returns the validated preimage as an owned `String`, or `None` if
/// the value is anything but a 64-char lowercase-hex string.
pub fn validate_preimage(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    if s.len() != 64 {
        return None;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return None;
    }
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn deadline_closes_a_socket_that_never_replies() {
        let path =
            std::env::temp_dir().join(format!("cln-timeout-{}.sock", crate::db::random_hex(8)));
        let listener = UnixListener::bind(&path).unwrap();
        let cleanup = path.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut request = String::new();
            socket.read_line(&mut request).await.unwrap();
            let mut byte = [0];
            assert_eq!(
                socket.read(&mut byte).await.unwrap(),
                0,
                "timeout must close the client socket"
            );
            std::fs::remove_file(cleanup).unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            call_with_timeout(&path, "getinfo", json!({}), Duration::from_millis(50)),
        )
        .await
        .expect("RPC did not respect its deadline");
        assert!(matches!(result, Err(CallErr::Transport(error))
            if error.to_string().contains("getinfo RPC timed out")));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn deadline_also_covers_a_blocked_request_write() {
        let path = std::env::temp_dir().join(format!(
            "cln-write-timeout-{}.sock",
            crate::db::random_hex(8)
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let cleanup = path.clone();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            // Do not drain the request: it must fill the Unix socket's send buffer.
            let _ = released.await;
            std::fs::remove_file(cleanup).unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            call_with_timeout(
                &path,
                "pay",
                json!({"bolt11": "x".repeat(2 * 1024 * 1024)}),
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("RPC write did not respect its deadline");
        assert!(matches!(result, Err(CallErr::Transport(error))
            if error.to_string().contains("pay RPC timed out")));
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deadline_preserves_success_and_structured_rpc_errors() {
        let (path, server) =
            crate::test_support::rpc(vec![("getinfo", json!({}), json!({"network": "regtest"}))])
                .await;
        let response = call_with_timeout(&path, "getinfo", json!({}), Duration::from_secs(2)).await;
        assert!(matches!(response, Ok(value) if value["network"] == "regtest"));
        server.await.unwrap();

        let path =
            std::env::temp_dir().join(format!("cln-error-{}.sock", crate::db::random_hex(8)));
        let listener = UnixListener::bind(&path).unwrap();
        let cleanup = path.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut request = String::new();
            socket.read_line(&mut request).await.unwrap();
            socket.get_mut().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":210,\"message\":\"pending\",\"data\":{\"attempt\":1}}}\n").await.unwrap();
            std::fs::remove_file(cleanup).unwrap();
        });
        let response = call_with_timeout(&path, "pay", json!({}), Duration::from_secs(2)).await;
        assert!(
            matches!(response, Err(CallErr::Rpc { code: 210, data, .. }) if data["attempt"] == 1)
        );
        server.await.unwrap();
    }

    #[test]
    fn fee_requires_a_bounded_consistent_sent_amount() {
        assert_eq!(
            payment_fee_msat(&json!({"amount_sent_msat": 1100}), 1000, 500).unwrap(),
            100
        );
        for response in [
            json!({}),
            json!({"amount_sent_msat": u64::MAX}),
            json!({"amount_sent_msat": 999}),
            json!({"amount_sent_msat": 1600}),
        ] {
            assert!(payment_fee_msat(&response, 1000, 500).is_err());
        }
    }
    use serde_json::json;

    #[test]
    fn validate_preimage_accepts_64_lowercase_hex() {
        let v = json!("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        assert_eq!(
            validate_preimage(&v).as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn validate_preimage_rejects_empty() {
        assert!(validate_preimage(&json!("")).is_none());
    }

    #[test]
    fn validate_preimage_rejects_wrong_length() {
        assert!(validate_preimage(&json!("0123")).is_none());
        // 63 chars
        assert!(validate_preimage(&json!(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde"
        ))
        .is_none());
        // 65 chars
        assert!(validate_preimage(&json!(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0"
        ))
        .is_none());
    }

    #[test]
    fn validate_preimage_rejects_uppercase() {
        // 64 hex chars but uppercase. Be strict — CLN emits lowercase
        // and accepting uppercase here would let mixed-case duplicates
        // exist as different stored strings.
        let v = json!("0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(validate_preimage(&v).is_none());
    }

    #[test]
    fn validate_preimage_rejects_non_hex() {
        // 64 chars, but contains 'g'.
        let v = json!("g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        assert!(validate_preimage(&v).is_none());
    }

    #[test]
    fn validate_preimage_rejects_non_string() {
        assert!(validate_preimage(&json!(null)).is_none());
        assert!(validate_preimage(&json!(123)).is_none());
        assert!(validate_preimage(&json!(true)).is_none());
        assert!(validate_preimage(&json!({})).is_none());
    }

    #[test]
    fn parse_msat_accepts_known_shapes() {
        assert_eq!(parse_msat(&json!(1000)), Some(1000));
        assert_eq!(parse_msat(&json!("1000msat")), Some(1000));
        assert_eq!(parse_msat(&json!("1000")), Some(1000));
        assert_eq!(parse_msat(&json!(null)), None);
        assert_eq!(parse_msat(&json!("garbage")), None);
    }
}
