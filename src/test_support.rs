// src/test_support.rs
// Disposable Unix-socket RPC fixtures: no running Lightning node is used by unit tests.

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

/// Each step asserts the actual method/params and returns the supplied result.
pub async fn rpc(steps: Vec<(&'static str, Value, Value)>) -> (PathBuf, JoinHandle<()>) {
    let path = std::env::temp_dir().join(format!("cln-hub-test-{}.sock", crate::db::random_hex(8)));
    let listener = UnixListener::bind(&path).unwrap();
    let cleanup = path.clone();
    let task = tokio::spawn(async move {
        for (method, params, result) in steps {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut request = String::new();
            socket.read_line(&mut request).await.unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], method);
            assert_eq!(request["params"], params);
            let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
            socket.get_mut().write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        }
        std::fs::remove_file(cleanup).unwrap();
    });
    (path, task)
}
