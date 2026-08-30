//! QR-code rendering for client peer configs — the wg-admin operator UX:
//! a phone WireGuard client scans the SVG and gets the full peer conf
//! without typing a base64-encoded private key.
//!
//! Thin wrapper over the `qrcode` crate. Returns SVG text the caller can
//! embed in a webd response, save as a file, or pipe to a terminal SVG
//! viewer. We do not also offer PNG or ASCII renderings yet; adding them
//! is a feature flag away if a consumer wants one.

use std::fmt;

/// Errors from [`render_qr_svg`].
#[derive(Debug)]
pub enum QrError {
    /// The encoder rejected the payload — most commonly `DataTooLong`
    /// (QR version 40 + ECC level L holds ~2953 bytes; a wg client
    /// conf typically clocks in under 800 chars, but explicit error
    /// surfacing matters for the long-`AllowedIPs` edge case).
    Encode(qrcode::types::QrError),
    /// Caller passed an empty string. The qrcode crate is happy to
    /// encode an empty payload, but a scannable-but-useless QR is a
    /// silent footgun if it ends up in a webd-served peer config; we
    /// refuse at this layer so an upstream bug surfaces eagerly.
    EmptyInput,
}

impl fmt::Display for QrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QrError::Encode(e) => write!(f, "qr encode failed: {e}"),
            QrError::EmptyInput => write!(f, "qr encode refused: empty input"),
        }
    }
}

impl std::error::Error for QrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            QrError::Encode(e) => Some(e),
            QrError::EmptyInput => None,
        }
    }
}

impl From<qrcode::types::QrError> for QrError {
    fn from(e: qrcode::types::QrError) -> Self {
        QrError::Encode(e)
    }
}

/// Render `text` as an SVG QR code at the qrcode crate's default
/// styling (1px dark modules on a white background, a 4-module quiet
/// zone). The output is a complete `<svg ...>...</svg>` document.
///
/// Intended for client peer configs (typically <1 KiB), but accepts any
/// text the underlying QR encoder can fit — error-corrected at the
/// crate's default level.
pub fn render_qr_svg(text: &str) -> Result<String, QrError> {
    if text.is_empty() {
        return Err(QrError::EmptyInput);
    }
    let code = qrcode::QrCode::new(text.as_bytes())?;
    Ok(code.render::<qrcode::render::svg::Color<'_>>().build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_non_empty_svg_for_typical_peer_conf() {
        let conf = "[Interface]\n\
                    PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=\n\
                    Address = 192.0.2.5/32\n\
                    \n\
                    [Peer]\n\
                    PublicKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
                    AllowedIPs = 0.0.0.0/0\n\
                    Endpoint = vpn.example:51820\n";
        let svg = render_qr_svg(conf).unwrap();
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"), "{svg}");
        assert!(svg.contains("</svg>"));
        // QR codes always contain finder squares; the SVG should be
        // substantial, not a near-empty placeholder.
        assert!(
            svg.len() > 256,
            "svg suspiciously short: {} bytes",
            svg.len()
        );
    }

    #[test]
    fn deterministic_output_for_same_input() {
        // qrcode's rendering is deterministic; this guards against an
        // accidental random padding step (some QR encoders mask
        // non-deterministically).
        let a = render_qr_svg("hello world").unwrap();
        let b = render_qr_svg("hello world").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_input_yields_distinct_output() {
        let a = render_qr_svg("hello").unwrap();
        let b = render_qr_svg("world").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn rejects_overlong_input() {
        // QR version 40 + ECC level L holds ~2953 bytes of binary data.
        // Pad past that and the encoder must surface DataTooLong.
        let huge = "x".repeat(8192);
        match render_qr_svg(&huge) {
            Err(QrError::Encode(qrcode::types::QrError::DataTooLong)) => {}
            other => panic!("expected DataTooLong, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_input() {
        // An empty QR scans to nothing useful and would silently mask an
        // upstream "generated an empty peer config" bug. Refuse eagerly.
        match render_qr_svg("") {
            Err(QrError::EmptyInput) => {}
            other => panic!("expected EmptyInput, got {other:?}"),
        }
    }
}
