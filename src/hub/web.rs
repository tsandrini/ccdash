//! The web UI's static files, embedded in the binary: the panel, its
//! service worker and the PWA manifest with its icons. `CCDASH_WEB_DIR`
//! serves them from disk instead, for live editing.

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'";

struct Asset {
    name: &'static str,
    mime: &'static str,
    bytes: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        name: "index.html",
        mime: "text/html; charset=utf-8",
        bytes: include_bytes!("../../web/index.html"),
    },
    Asset {
        name: "app.js",
        mime: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../web/app.js"),
    },
    Asset {
        name: "app.css",
        mime: "text/css; charset=utf-8",
        bytes: include_bytes!("../../web/app.css"),
    },
    Asset {
        name: "icon.svg",
        mime: "image/svg+xml",
        bytes: include_bytes!("../../web/icon.svg"),
    },
    Asset {
        name: "manifest.webmanifest",
        mime: "application/manifest+json",
        bytes: include_bytes!("../../web/manifest.webmanifest"),
    },
    Asset {
        name: "sw.js",
        mime: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../web/sw.js"),
    },
    Asset {
        name: "icon-192.png",
        mime: "image/png",
        bytes: include_bytes!("../../web/icon-192.png"),
    },
    Asset {
        name: "icon-512.png",
        mime: "image/png",
        bytes: include_bytes!("../../web/icon-512.png"),
    },
    Asset {
        name: "icon-maskable-512.png",
        mime: "image/png",
        bytes: include_bytes!("../../web/icon-maskable-512.png"),
    },
];

pub async fn index() -> Response {
    serve("index.html")
}

pub async fn asset(Path(name): Path<String>) -> Response {
    serve(&name)
}

fn serve(name: &str) -> Response {
    let Some(asset) = ASSETS.iter().find(|a| a.name == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let bytes = match std::env::var_os("CCDASH_WEB_DIR") {
        Some(dir) => match std::fs::read(std::path::Path::new(&dir).join(asset.name)) {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        },
        None => asset.bytes.to_vec(),
    };
    (
        [
            (header::CONTENT_TYPE, asset.mime),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        bytes,
    )
        .into_response()
}
