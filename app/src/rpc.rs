//! Substrate JSON-RPC analytics client.
//!
//! Connects to the node's WebSocket RPC on `ws://127.0.0.1:9944` (localhost
//! inside the app, so no network permission fuss) and feeds the dashboard:
//!
//! - `system_name`, `system_version`, `system_chain` — once on connect
//! - `chain_subscribeNewHeads`       — live best block height + hash
//! - `chain_subscribeFinalizedHeads` — finalized height
//! - `system_health` polled every 2s — peers / syncing state

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use dioxus::prelude::*;

use crate::node::{push_log, RUNNING};
use crate::{STATUS, STATUS_RPC};

const RPC_URL: &str = "ws://127.0.0.1:9944";

/// Guards against spawning a second RPC client (dev hot-reload remounts).
static RPC_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What kind of response we expect for a given JSON-RPC request id.
#[derive(Clone, Copy)]
enum Pending {
    Name,
    Version,
    Chain,
    Health,
    SubHead,
    SubFinal,
}

/// RPC session owner. Retries connecting while the node is starting, keeps
/// subscriptions alive, reconnects on error, and exits when the node stops.
pub fn spawn_client() {
    spawn(async move {
        // single-client guard (dev hot-reload remounts re-run the bootstrap)
        if RPC_STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        while RUNNING.load(Ordering::SeqCst) {
            if run_session().await {
                RPC_STARTED.store(false, Ordering::SeqCst);
                return; // node stopped, session ended normally
            }
            *STATUS_RPC.write() = "connecting…".into();
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
        RPC_STARTED.store(false, Ordering::SeqCst);
    });
}

/// One connection lifecycle. `true` = node went away (clean exit);
/// `false` = connection failed, caller retries.
async fn run_session() -> bool {
    // The node takes a while to open its RPC (state DB open, WASM init).
    let (ws, _resp) = loop {
        if !RUNNING.load(Ordering::SeqCst) {
            return true;
        }
        match connect_async(RPC_URL).await {
            Ok(x) => break x,
            Err(_) => {
                *STATUS_RPC.write() = "waiting for node rpc…".into();
                tokio::time::sleep(Duration::from_millis(750)).await;
            }
        }
    };
    *STATUS_RPC.write() = RPC_URL.into();
    push_log("rpc connected");

    let ws: WebSocketStream<MaybeTlsStream<TcpStream>> = ws;
    let (mut sink, mut stream) = ws.split();

    let mut next_id: u64 = 1;
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut subs: HashMap<Value, &'static str> = HashMap::new();

    async fn send(
        sink: &mut (impl futures_util::Sink<Message> + Unpin + std::marker::Send),
        pending: &mut HashMap<u64, Pending>,
        next_id: &mut u64,
        method: &str,
        kind: Pending,
        params: Value,
    ) {
        let id = *next_id;
        *next_id += 1;
        pending.insert(id, kind);
        let msg = Message::text(
            serde_json::to_string(&json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params,
            }))
            .unwrap(),
        );
        let _ = sink.send(msg).await;
    }

    send(&mut sink, &mut pending, &mut next_id, "system_name", Pending::Name, json!([])).await;
    send(&mut sink, &mut pending, &mut next_id, "system_version", Pending::Version, json!([])).await;
    send(&mut sink, &mut pending, &mut next_id, "system_chain", Pending::Chain, json!([])).await;
    send(&mut sink, &mut pending, &mut next_id, "system_health", Pending::Health, json!([])).await;
    send(
        &mut sink, &mut pending, &mut next_id,
        "chain_subscribeNewHeads", Pending::SubHead, json!([]),
    )
    .await;
    send(
        &mut sink, &mut pending, &mut next_id,
        "chain_subscribeFinalizedHeads", Pending::SubFinal, json!([]),
    )
    .await;

    let mut health_tick = tokio::time::interval(Duration::from_secs(2));
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // node lifecycle gate — poll cheaply
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if !RUNNING.load(Ordering::SeqCst) {
                    return true;
                }
            }

            // health poll
            _ = health_tick.tick() => {
                if RUNNING.load(Ordering::SeqCst) {
                    send(&mut sink, &mut pending, &mut next_id, "system_health", Pending::Health, json!([])).await;
                }
            }

            msg = stream.next() => {
                let Some(Ok(msg)) = msg else {
                    if RUNNING.load(Ordering::SeqCst) {
                        push_log("rpc disconnected, reconnecting");
                        return false;
                    }
                    return true;
                };
                let Message::Text(txt) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };

                if let Some(method) = v.get("method").and_then(Value::as_str) {
                    // subscription notification: params.subscription → kind
                    let sub_id = v.pointer("/params/subscription").cloned().unwrap_or(Value::Null);
                    let kind = subs.get(&sub_id).copied();
                    let result = v.pointer("/params/result");
                    match (kind, method, result) {
                        (Some("head"), "chain_newHead", Some(r)) => {
                            STATUS.write().best_number = r.get("number").and_then(parse_hex_u32);
                            STATUS.write().best_hash = r
                                .get("hash")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                        }
                        (Some("final"), "chain_finalizedHead", Some(r)) => {
                            STATUS.write().finalized_number = r.get("number").and_then(parse_hex_u32);
                            // read into a plain bool first: a read guard in the
                            // `if` condition would still be alive for the body
                            let no_hash_yet = STATUS.read().best_hash.is_empty();
                            if no_hash_yet {
                                STATUS.write().best_hash = r
                                    .get("hash")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                            }
                        }
                        _ => {}
                    }
                } else if let Some(kind) = v.get("id").and_then(Value::as_u64).and_then(|i| pending.remove(&i)) {
                    match kind {
                        Pending::Name => {
                            STATUS.write().name = as_text(v.get("result"));
                        }
                        Pending::Version => {
                            STATUS.write().version = as_text(v.get("result"));
                        }
                        Pending::Chain => {
                            STATUS.write().chain = as_text(v.get("result"));
                        }
                        Pending::Health => {
                            if let Some(h) = v.get("result") {
                                STATUS.write().peers = h.get("peers").and_then(Value::as_i64).unwrap_or(0);
                                STATUS.write().is_syncing =
                                    h.get("isSyncing").and_then(Value::as_bool).unwrap_or(false);
                            }
                        }
                        Pending::SubHead => {
                            subs.insert(v.get("result").cloned().unwrap_or(Value::Null), "head");
                        }
                        Pending::SubFinal => {
                            subs.insert(v.get("result").cloned().unwrap_or(Value::Null), "final");
                        }
                    }
                }
            }
        }
    }
}

fn as_text(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_string()
}

fn parse_hex_u32(v: &Value) -> Option<u32> {
    let s = v.as_str()?;
    let s = s.trim_start_matches("0x");
    u32::from_str_radix(s, 16).ok()
}
