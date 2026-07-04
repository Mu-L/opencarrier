//! Embedded static assets served as compile-time-included data.
//!
//! Static files (logo, favicon, share page, PWA assets, KaTeX fonts) are
//! bundled into the binary via `include_str!`/`include_bytes!` for
//! single-binary deployment.

use axum::extract::Path;
use axum::http::header;
use axum::response::{IntoResponse, Redirect};

/// Embedded logo PNG for single-binary deployment.
const LOGO_PNG: &[u8] = include_bytes!("../static/logo.png");

/// Embedded favicon ICO for browser tabs.
const FAVICON_ICO: &[u8] = include_bytes!("../static/favicon.ico");

/// Embedded PWA manifest for installable web app support.
const MANIFEST_JSON: &str = include_str!("../static/manifest.json");

/// Embedded service worker for PWA support.
const SW_JS: &str = include_str!("../static/sw.js");

/// Embedded KaTeX font files — all .woff2 variants bundled at compile time.
const KATEX_FONT_AMS_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_AMS-Regular.woff2");
const KATEX_FONT_CALIGRAPHIC_BOLD: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Caligraphic-Bold.woff2");
const KATEX_FONT_CALIGRAPHIC_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Caligraphic-Regular.woff2");
const KATEX_FONT_FRAKTUR_BOLD: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Fraktur-Bold.woff2");
const KATEX_FONT_FRAKTUR_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Fraktur-Regular.woff2");
const KATEX_FONT_MAIN_BOLD: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Main-Bold.woff2");
const KATEX_FONT_MAIN_BOLDITALIC: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Main-BoldItalic.woff2");
const KATEX_FONT_MAIN_ITALIC: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Main-Italic.woff2");
const KATEX_FONT_MAIN_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Main-Regular.woff2");
const KATEX_FONT_MATH_BOLDITALIC: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Math-BoldItalic.woff2");
const KATEX_FONT_MATH_ITALIC: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Math-Italic.woff2");
const KATEX_FONT_SANSSERIF_BOLD: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_SansSerif-Bold.woff2");
const KATEX_FONT_SANSSERIF_ITALIC: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_SansSerif-Italic.woff2");
const KATEX_FONT_SANSSERIF_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_SansSerif-Regular.woff2");
const KATEX_FONT_SCRIPT_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Script-Regular.woff2");
const KATEX_FONT_SIZE1_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Size1-Regular.woff2");
const KATEX_FONT_SIZE2_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Size2-Regular.woff2");
const KATEX_FONT_SIZE3_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Size3-Regular.woff2");
const KATEX_FONT_SIZE4_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Size4-Regular.woff2");
const KATEX_FONT_TYPEWRITER_REGULAR: &[u8] =
    include_bytes!("../static/vendor/katex-fonts/KaTeX_Typewriter-Regular.woff2");

// ── Route handlers ──────────────────────────────────────────────────────

/// GET /logo.png — Serve the Carrier logo.
pub async fn logo_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        LOGO_PNG,
    )
}

/// GET /favicon.ico — Serve the Carrier favicon.
pub async fn favicon_ico() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/x-icon"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        FAVICON_ICO,
    )
}

/// GET /manifest.json — Serve the PWA web app manifest.
pub async fn manifest_json() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        MANIFEST_JSON,
    )
}

/// GET /sw.js — Serve the PWA service worker.
pub async fn sw_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        SW_JS,
    )
}

/// GET /katex-fonts/:name — Serve a KaTeX font file (.woff2 only).
pub async fn katex_font(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::response::Response<axum::body::Body> {
    let (data, content_type) = match name.as_str() {
        "KaTeX_AMS-Regular.woff2" => (KATEX_FONT_AMS_REGULAR, "font/woff2"),
        "KaTeX_Caligraphic-Bold.woff2" => (KATEX_FONT_CALIGRAPHIC_BOLD, "font/woff2"),
        "KaTeX_Caligraphic-Regular.woff2" => (KATEX_FONT_CALIGRAPHIC_REGULAR, "font/woff2"),
        "KaTeX_Fraktur-Bold.woff2" => (KATEX_FONT_FRAKTUR_BOLD, "font/woff2"),
        "KaTeX_Fraktur-Regular.woff2" => (KATEX_FONT_FRAKTUR_REGULAR, "font/woff2"),
        "KaTeX_Main-Bold.woff2" => (KATEX_FONT_MAIN_BOLD, "font/woff2"),
        "KaTeX_Main-BoldItalic.woff2" => (KATEX_FONT_MAIN_BOLDITALIC, "font/woff2"),
        "KaTeX_Main-Italic.woff2" => (KATEX_FONT_MAIN_ITALIC, "font/woff2"),
        "KaTeX_Main-Regular.woff2" => (KATEX_FONT_MAIN_REGULAR, "font/woff2"),
        "KaTeX_Math-BoldItalic.woff2" => (KATEX_FONT_MATH_BOLDITALIC, "font/woff2"),
        "KaTeX_Math-Italic.woff2" => (KATEX_FONT_MATH_ITALIC, "font/woff2"),
        "KaTeX_SansSerif-Bold.woff2" => (KATEX_FONT_SANSSERIF_BOLD, "font/woff2"),
        "KaTeX_SansSerif-Italic.woff2" => (KATEX_FONT_SANSSERIF_ITALIC, "font/woff2"),
        "KaTeX_SansSerif-Regular.woff2" => (KATEX_FONT_SANSSERIF_REGULAR, "font/woff2"),
        "KaTeX_Script-Regular.woff2" => (KATEX_FONT_SCRIPT_REGULAR, "font/woff2"),
        "KaTeX_Size1-Regular.woff2" => (KATEX_FONT_SIZE1_REGULAR, "font/woff2"),
        "KaTeX_Size2-Regular.woff2" => (KATEX_FONT_SIZE2_REGULAR, "font/woff2"),
        "KaTeX_Size3-Regular.woff2" => (KATEX_FONT_SIZE3_REGULAR, "font/woff2"),
        "KaTeX_Size4-Regular.woff2" => (KATEX_FONT_SIZE4_REGULAR, "font/woff2"),
        "KaTeX_Typewriter-Regular.woff2" => (KATEX_FONT_TYPEWRITER_REGULAR, "font/woff2"),
        _ => {
            return axum::response::Response::builder()
                .status(axum::http::StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(axum::body::Body::from("font not found"))
                .unwrap();
        }
    };
    axum::response::Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(axum::body::Body::from(data))
        .unwrap()
}

/// Serve the public share page.
pub async fn share_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        SHARE_HTML,
    )
}

/// Domain verification file content.
const VERIFICATION_TXT: &str = "16e0caa15c3973ff558268d2d4f4df2f63e86385";

/// Serve domain verification file.
pub async fn verification_txt() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        VERIFICATION_TXT,
    )
}

/// Redirect `/v/{phone}` to `tel:{phone}` so tapping the link in WeChat
/// triggers the phone dialer (no page rendered — just an HTTP 302).
pub async fn vcard_redirect(Path(phone): Path<String>) -> Redirect {
    Redirect::to(&format!("tel:{}", phone))
}

// ── Compile-time assembled HTML ─────────────────────────────────────────

/// Share page HTML — assembled at compile time with QR code library inlined.
const SHARE_HTML: &str = concat!(
    include_str!("../static/share.html"),
    "<script>\n",
    include_str!("../static/vendor/qrcode.min.js"),
    "\n</script>\n",
    "</body>\n</html>"
);
