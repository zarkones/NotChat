//! NotChat UI entry (desktop / mobile via features).

#[cfg(any(feature = "desktop", feature = "mobile"))]
fn main() {
    onion_chat::ui::launch_app();
}

#[cfg(not(any(feature = "desktop", feature = "mobile")))]
fn main() {
    eprintln!(
        "onion-chat UI requires --features desktop or --features mobile\n\
         For Tor/protocol without GUI: cargo run --bin headless"
    );
    std::process::exit(1);
}
