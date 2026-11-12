//! Solochain Node — Android Dioxus dashboard for a bundled polkadot-sdk
//! solochain node.
//!
//! The node runs as a child process (the aarch64 binary packaged in the APK's
//! jniLibs), serving JSON-RPC on ws://127.0.0.1:9944. This app starts/stops
//! it and renders live running-status analytics.

use dioxus::prelude::*;

mod node;
mod rpc;

// ---------------------------------------------------------------------------
// global state
// ---------------------------------------------------------------------------

#[derive(Clone, Default, PartialEq)]
pub struct Status {
    /// idle | starting | running | stopped | crashed | error
    pub phase: String,
    pub error: String,
    pub name: String,
    pub version: String,
    pub chain: String,
    pub best_number: Option<u32>,
    pub best_hash: String,
    pub finalized_number: Option<u32>,
    pub peers: i64,
    pub is_syncing: bool,
    pub db_bytes: u64,
    pub free_bytes: u64,
    pub started_at: Option<u64>,
    pub uptime_secs: u64,
}

pub static STATUS: GlobalSignal<Status> = Signal::global(Status::default);
pub static LOGS: GlobalSignal<String> = Signal::global(String::new);
pub static STATUS_RPC: GlobalSignal<String> = Signal::global(String::new);

fn log_line(line: &str) {
    // dioxus-logger routes to logcat on Android
    dioxus::logger::tracing::info!(target: "node", "{line}");
}

/// Must match Dioxus.toml [bundle] identifier — used for the JNI intent that
/// starts the foreground service (Intent.setClassName).
pub const APP_PACKAGE: &str = "dev.solochain.node";

fn status() -> Signal<Status> {
    STATUS.signal()
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let mut tick = use_signal(|| 0u64);
    // transient "✓ copied" feedback for the log-copy button
    let mut copied = use_signal(|| false);

    // One-shot bootstrap: storage watcher + uptime ticker, then start node + rpc.
    use_effect(move || {
        node::spawn_storage_watcher();
        node::spawn_uptime_ticker();
        node::spawn_start();
        rpc::spawn_client();
    });

    // 1 Hz re-render tick so uptime/heartbeat stays live.
    use_effect(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                tick += 1;
            }
        });
    });

    let st = STATUS.read();
    let rpc_note = STATUS_RPC.read().clone();
    let db_note = format!(
        "Chain database is at {} — consider resetting chain data.",
        node::fmt_bytes(st.db_bytes)
    );
    let _ = tick(); // subscribe

    let (dot_class, phase_label): (&str, &str) = match st.phase.as_str() {
        "running" => ("bg-emerald-400 animate-pulse", "RUNNING"),
        "starting" => ("bg-amber-400 animate-pulse", "STARTING"),
        "stopped" => ("bg-slate-500", "STOPPED"),
        "crashed" => ("bg-rose-500", "CRASHED"),
        "error" => ("bg-rose-500", "ERROR"),
        _ => ("bg-slate-500", "IDLE"),
    };

    let running = st.phase == "running";
    let busy = running || st.phase == "starting";

    rsx! {
        document::Stylesheet { href: asset!("/assets/tailwind.css") }

        div { class: "min-h-screen bg-slate-950 text-slate-100 antialiased",

            // ---------------- header ----------------
            header { class: "border-b border-slate-800 bg-slate-900/60 backdrop-blur px-5 py-4",
                div { class: "mx-auto max-w-3xl flex items-center justify-between gap-3",
                    div { class: "flex items-center gap-3",
                        span { class: "inline-block h-3 w-3 rounded-full {dot_class}" }
                        div {
                            h1 { class: "text-lg font-semibold tracking-tight", "Solochain Node" }
                            p { class: "text-xs text-slate-400",
                                "{st.name} {st.version} · {st.chain} · rpc: {rpc_note}"
                            }
                        }
                    }
                    span { class: "rounded-full border border-slate-700 bg-slate-800 px-3 py-1 text-[11px] font-mono tracking-widest text-slate-300",
                        "{phase_label}"
                    }
                }
            }

            main { class: "mx-auto max-w-3xl px-5 py-6 flex flex-col gap-5",

                // ---------------- error / storage banner ----------------
                { (!st.error.is_empty()).then(|| rsx! {
                    div { class: "rounded-xl border border-rose-800 bg-rose-950/60 px-4 py-3 text-sm text-rose-200",
                        "{st.error}"
                    }
                }) }
                { (st.db_bytes > node::MAX_DB_BYTES * 3 / 4).then(|| rsx! {
                    div { class: "rounded-xl border border-amber-700 bg-amber-950/50 px-4 py-3 text-sm text-amber-200",
                        "{db_note}"
                    }
                }) }

                // ---------------- stat cards ----------------
                div { class: "grid grid-cols-2 gap-3 sm:grid-cols-3",

                    Card {
                        label: "Best block",
                        value: st.best_number.map(|n| n.to_string()).unwrap_or_else(|| "—".into()),
                        sub: st.best_hash.chars().take(12).collect::<String>(),
                        accent: true,
                    }
                    Card {
                        label: "Finalized",
                        value: st.finalized_number.map(|n| n.to_string()).unwrap_or_else(|| "—".into()),
                        sub: "last finalized head",
                        accent: false,
                    }
                    Card {
                        label: "Peers",
                        value: st.peers.to_string(),
                        sub: if st.is_syncing { "syncing" } else { "idle / up to date" },
                        accent: false,
                    }
                    Card {
                        label: "Uptime",
                        value: fmt_uptime(st.uptime_secs),
                        sub: "since node start",
                        accent: false,
                    }
                    Card {
                        label: "Chain DB",
                        value: node::fmt_bytes(st.db_bytes),
                        sub: format!("free: {}", node::fmt_bytes(st.free_bytes)),
                        accent: false,
                    }
                    Card {
                        label: "RPC",
                        value: "9944",
                        sub: "ws://127.0.0.1:9944",
                        accent: false,
                    }
                }

                // ---------------- controls ----------------
                div { class: "flex flex-wrap gap-3",
                    if running {
                        button {
                            class: "rounded-xl bg-slate-800 px-5 py-3 text-sm font-semibold text-slate-100 border border-slate-700 hover:bg-slate-700 active:scale-95 transition",
                            onclick: move |_| node::stop(),
                            "■ Stop node"
                        }
                    } else {
                        button {
                            class: "rounded-xl bg-indigo-600 px-5 py-3 text-sm font-semibold text-white hover:bg-indigo-500 active:scale-95 transition disabled:opacity-50",
                            disabled: busy,
                            onclick: move |_| node::spawn_start(),
                            "▶ Start node"
                        }
                    }
                    button {
                        class: "rounded-xl bg-slate-800 px-5 py-3 text-sm font-semibold text-slate-100 border border-slate-700 hover:bg-slate-700 active:scale-95 transition disabled:opacity-50",
                        disabled: !running,
                        onclick: move |_| {
                            node::stop();
                            spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                                node::spawn_start();
                            });
                        },
                        "⟳ Restart"
                    }
                    button {
                        class: "rounded-xl bg-rose-900/70 px-5 py-3 text-sm font-semibold text-rose-100 border border-rose-800 hover:bg-rose-800/80 active:scale-95 transition",
                        disabled: busy,
                        onclick: move |_| node::reset_chain(),
                        "⌫ Reset chain data"
                    }
                }

                // ---------------- logs ----------------
                div { class: "rounded-2xl border border-slate-800 bg-black/60 overflow-hidden",
                    div { class: "flex items-center justify-between px-4 py-2 border-b border-slate-800",
                        span { class: "text-xs font-semibold tracking-widest text-slate-400", "NODE LOG" }
                        div { class: "flex items-center gap-2",
                            span { class: "text-[10px] text-slate-500 font-mono", "ring buffer, 300 lines" }
                            button {
                                class: "text-[10px] font-mono px-2 py-1 rounded-md border border-slate-700 text-slate-300 hover:bg-slate-800 active:scale-95 transition",
                                onclick: move |_| {
                                    copy_logs_to_clipboard();
                                    copied.set(true);
                                    spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                        copied.set(false);
                                    });
                                },
                                if copied() { "✓ copied" } else { "⧉ copy" }
                            }
                        }
                    }
                    pre { class: "max-h-64 overflow-y-auto px-4 py-3 text-[11px] leading-relaxed font-mono text-slate-300 whitespace-pre-wrap break-all",
                        { LOGS.read().clone() }
                    }
                }

                p { class: "text-[11px] text-slate-500 text-center",
                    "dev mode: blocks are instant-sealed per transaction, so height stays flat until a tx is submitted"
                }
            }
        }
    }
}

#[component]
fn Card(label: String, value: String, sub: String, accent: bool) -> Element {
    let value_class = if accent {
        "text-2xl font-bold text-indigo-300"
    } else {
        "text-2xl font-bold text-slate-100"
    };
    rsx! {
        div { class: "rounded-2xl border border-slate-800 bg-slate-900/50 px-4 py-3 flex flex-col gap-1",
            span { class: "text-[11px] font-semibold tracking-widest text-slate-500 uppercase", "{label}" }
            span { class: "{value_class}", "{value}" }
            span { class: "text-[11px] text-slate-500 font-mono truncate", "{sub}" }
        }
    }
}

fn fmt_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

// ---------------------------------------------------------------------------
// clipboard: copy the node log ring buffer
// ---------------------------------------------------------------------------
//
// Dioxus 0.7 has no imperative clipboard-write API, so we copy through the
// webview's JS. `navigator.clipboard.writeText` is the modern path but needs a
// secure context; the `execCommand("copy")` fallback works in the Android
// webview regardless, so we chain the two.

/// Copy the current contents of the node log ring buffer to the clipboard.
fn copy_logs_to_clipboard() {
    let text = LOGS.read().clone();
    if text.trim().is_empty() {
        return;
    }
    let _ = dioxus::document::eval(&clipboard_write_js(&text));
}

/// Escape a Rust string for embedding inside a double-quoted JS string
/// literal (backslashes, quotes, newlines, and other control chars).
fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Build the IIFE that writes `text` to the clipboard, trying the async
/// Clipboard API first and falling back to a hidden-textarea + execCommand
/// (required on Android webviews where navigator.clipboard may be blocked).
fn clipboard_write_js(text: &str) -> String {
    let escaped = js_escape(text);
    format!(
        r#"(function() {{
    var t = "{escaped}";
    function fallback() {{
        var ta = document.createElement("textarea");
        ta.value = t;
        ta.style.position = "fixed";
        ta.style.opacity = "0";
        document.body.appendChild(ta);
        ta.focus();
        ta.select();
        try {{ document.execCommand("copy"); }} catch (e) {{}}
        document.body.removeChild(ta);
    }}
    if (navigator.clipboard && navigator.clipboard.writeText) {{
        navigator.clipboard.writeText(t).catch(fallback);
    }} else {{
        fallback();
    }}
}})()"#
    )
}
