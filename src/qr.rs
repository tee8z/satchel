//! QR codes as inline SVG, so pages need no image endpoint or script.

use maud::{Markup, PreEscaped};
use qrcode::{Color, EcLevel, QrCode};

/// One `<svg>` with a single path of dark modules and a four-module quiet zone.
pub(crate) fn svg(data: &str, label: &str) -> Markup {
    let Ok(code) = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L) else {
        return PreEscaped(String::new());
    };
    let width = code.width();
    let size = width + 8;
    let mut path = String::new();
    for (index, color) in code.to_colors().into_iter().enumerate() {
        if color == Color::Dark {
            path.push_str(&format!("M{} {}h1v1h-1z", index % width + 4, index / width + 4));
        }
    }
    let label = maud::html! { (label) }.into_string();
    PreEscaped(format!(
        r##"<svg class="qr" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" role="img" aria-label="{label}" shape-rendering="crispEdges"><rect width="{size}" height="{size}" fill="#fff"/><path d="{path}" fill="#000"/></svg>"##
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_an_svg() {
        let svg = svg("LIGHTNING:LNTBS1TEST", "Invoice <QR>").into_string();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("aria-label=\"Invoice &lt;QR&gt;\""));
        assert!(svg.contains("h1v1h-1z"));
    }
}
