//! Headless / debug CLI: identity, DB, QR, optional onion service.
//!
//! Usage:
//!   headless              — start onion + serve /v1/* (default)
//!   headless init         — open DB, print identity + QR URI, exit
//!   headless demo-crypto  — seal/sign roundtrip self-test
//!   headless qr           — print current invite URI

use std::sync::mpsc;
use std::time::Duration;

use onion_chat::crypto::{now_unix_ms, open_message, seal_message, sign, verify_sig};
use onion_chat::default_data_root;
use onion_chat::onion::{open_app_state, spawn_onion_thread, OnionEvent};
use onion_chat::protocol::{Invite, IntroFrame, WIRE_VER};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("onion");

    let data = default_data_root();
    eprintln!("data dir: {}", data.display());

    match cmd {
        "init" | "qr" => {
            let shared = open_app_state(&data).expect("open state");
            let ident = shared.identity.lock().expect("lock");
            eprintln!("nick: {}", ident.nick);
            eprintln!(
                "pk: {}",
                onion_chat::protocol::b64url_encode(&ident.public_key_bytes())
            );
            match &ident.onion_id {
                Some(id) => eprintln!("id (56-char): {id}"),
                None => eprintln!("id (56-char): (not yet — start onion once to bind Tor address)"),
            }
            match Invite::from_identity(&ident) {
                Ok(uri) => {
                    println!("{uri}");
                    eprintln!("(invite URI on stdout)");
                }
                Err(e) => eprintln!("QR URI unavailable until onion id known: {e}"),
            }
        }
        "demo-crypto" => {
            let shared = open_app_state(&data).expect("open state");
            let a = shared.identity.lock().expect("lock").clone();
            let b = onion_chat::identity::Identity::generate("peer");
            let ts = now_unix_ms();
            let body = b"demo";
            let sig = sign(&a.signing, WIRE_VER, "intro", ts, body);
            verify_sig(&a.verifying_key(), WIRE_VER, "intro", ts, body, &sig)
                .expect("verify");
            let wire = seal_message(&a.signing, &b.verifying_key(), b"hello sealed")
                .expect("seal");
            let plain = open_message(&b.signing, &a.verifying_key(), &wire).expect("open");
            assert_eq!(plain, b"hello sealed");

            // Frame build if onion id present; else synthetic.
            let mut id = a.clone();
            if id.onion_id.is_none() {
                id.onion_id = Some(
                    "px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd".into(),
                );
            }
            let frame = IntroFrame::build(&id, &b.invite_nonce, ts).expect("intro");
            let v = frame.verify_and_parse().expect("intro verify");
            eprintln!("demo-crypto OK — intro from_id={}", v.from_id);
            eprintln!("sealed roundtrip OK ({} wire bytes)", wire.len());
        }
        "onion" | _ => {
            let shared = open_app_state(&data).expect("open state");
            {
                let ident = shared.identity.lock().expect("lock");
                eprintln!("identity pk ready; nick={:?}", ident.nick);
            }
            eprintln!("Starting Arti onion + /v1 HTTP (headless)…");
            eprintln!("(Bootstrap can take 30–120s on first run.)");
            eprintln!("Ctrl+C to stop. Auto-retries on failure.");

            let (tx, rx) = mpsc::channel::<OnionEvent>();
            let _cmd = spawn_onion_thread(tx, shared.clone(), data);

            let mut onion = None::<String>;
            loop {
                match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(OnionEvent::Status(s)) => eprintln!("[status] {s}"),
                    Ok(OnionEvent::OnionAddress(addr)) => {
                        eprintln!("[onion]  {addr}");
                        onion = Some(addr);
                    }
                    Ok(OnionEvent::OnionId(id)) => {
                        eprintln!("[id]     {id}");
                        if let Ok(ident) = shared.identity.lock() {
                            if let Ok(uri) = Invite::from_identity(&ident) {
                                eprintln!("[qr]     {uri}");
                            }
                        }
                    }
                    Ok(OnionEvent::Ready) => {
                        eprintln!("[ready]  serving GET /v1/health , POST /v1/intro|ack|msg");
                        if let Some(addr) = &onion {
                            eprintln!("         torsocks curl http://{addr}/v1/health");
                        }
                    }
                    Ok(OnionEvent::Down(s)) => eprintln!("[down]   {s}"),
                    Ok(OnionEvent::Error(e)) => eprintln!("[error]  {e}"),
                    Ok(OnionEvent::DbChanged) => eprintln!("[db]     changed"),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        eprintln!("Onion thread ended.");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}
