//! The secure phone link: lets the Seam phone app talk to this computer over Wi-Fi,
//! without USB debugging. Protocol: `protocol/PROTOCOL.md`.

pub mod client;
pub mod protocol;
pub mod server;
pub mod store;

pub use protocol::{Message, PairingInfo, PhoneNotification};
pub use server::{local_ip, LinkEvent, LinkServer};
pub use store::PairedDevice;

/// The pairing link as an SVG QR code.
pub fn qr_svg(data: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
mod tests {
    #[test]
    fn qr_is_svg() {
        let svg = super::qr_svg("seam://pair?v=1").unwrap();
        assert!(svg.contains("<svg"));
    }
}
