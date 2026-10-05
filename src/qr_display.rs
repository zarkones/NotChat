//! Invite QR image generation (display side).
//!
//! Uses the `qrcode` crate → SVG, then a data-URI for `<img src=…>`.
//! Compiles for desktop and `aarch64-linux-android` (no native image deps).

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use qrcode::render::svg;
use qrcode::QrCode;

/// Render an invite URI as an SVG QR code data URI for Profile display.
pub fn invite_qr_data_uri(invite_uri: &str) -> Result<String> {
    let uri = invite_uri.trim();
    if uri.is_empty() {
        anyhow::bail!("empty invite URI");
    }
    let code = QrCode::new(uri.as_bytes()).context("encode QR")?;
    let svg = code
        .render::<svg::Color>()
        .dark_color(svg::Color("#0b1020"))
        .light_color(svg::Color("#e7ecf3"))
        .min_dimensions(256, 256)
        .quiet_zone(true)
        .build();
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        STANDARD.encode(svg.as_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_onionchat_uri() {
        let uri = "onionchat:v1?id=px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd&pk=x&n=y&nick=Ada";
        let data = invite_qr_data_uri(uri).unwrap();
        assert!(data.starts_with("data:image/svg+xml;base64,"));
        assert!(data.len() > 100);
    }
}
