//! Static files compiled into the binary and served at content-hashed URLs,
//! so browsers can cache them for a year.

use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};

use crate::util::sha256;

struct Asset {
    name: &'static str,
    content_type: &'static str,
    body: &'static [u8],
    url: String,
}

fn asset(name: &'static str, content_type: &'static str, body: &'static [u8]) -> Asset {
    let hash = hex::encode(&sha256(body)[..6]);
    let (stem, extension) = name.split_once('.').unwrap_or((name, ""));
    Asset {
        name,
        content_type,
        body,
        url: format!("/assets/{stem}.{hash}.{extension}"),
    }
}

static ASSETS: LazyLock<[Asset; 3]> = LazyLock::new(|| {
    [
        asset(
            "app.css",
            "text/css; charset=utf-8",
            include_bytes!("../../assets/app.css"),
        ),
        asset(
            "app.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../assets/app.js"),
        ),
        asset(
            "htmx.min.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../assets/vendor/htmx/4.0.0/htmx.min.js"),
        ),
    ]
});

/// The web app manifest and its icons. The manifest names the icons by plain
/// path, so these are also served without the hash, with a shorter cache.
static APP_ICONS: LazyLock<[Asset; 6]> = LazyLock::new(|| {
    [
        asset(
            "manifest.webmanifest",
            "application/manifest+json",
            include_bytes!("../../assets/manifest.webmanifest"),
        ),
        asset("icon.svg", "image/svg+xml", include_bytes!("../../assets/icon.svg")),
        asset("icon-192.png", "image/png", include_bytes!("../../assets/icon-192.png")),
        asset("icon-512.png", "image/png", include_bytes!("../../assets/icon-512.png")),
        asset(
            "icon-maskable-512.png",
            "image/png",
            include_bytes!("../../assets/icon-maskable-512.png"),
        ),
        asset(
            "apple-touch-icon.png",
            "image/png",
            include_bytes!("../../assets/apple-touch-icon.png"),
        ),
    ]
});

fn all() -> impl Iterator<Item = &'static Asset> {
    ASSETS.iter().chain(APP_ICONS.iter())
}

/// The hashed URL for a file name such as `app.css`.
pub(crate) fn url(name: &str) -> &'static str {
    all()
        .find(|asset| asset.name == name)
        .map_or("/", |asset| asset.url.as_str())
}

pub(crate) async fn serve(Path(file): Path<String>) -> Response {
    let path = format!("/assets/{file}");
    let found = match all().find(|asset| asset.url == path) {
        Some(asset) => Some((asset, "public, max-age=31536000, immutable")),
        None => APP_ICONS
            .iter()
            .find(|asset| asset.name == file)
            .map(|asset| (asset, "public, max-age=86400")),
    };
    match found {
        Some((asset, cache)) => {
            ([(CONTENT_TYPE, asset.content_type), (CACHE_CONTROL, cache)], asset.body).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_carry_a_content_hash() {
        let css = url("app.css");
        assert!(css.starts_with("/assets/app.") && css.ends_with(".css") && css.len() > "/assets/app..css".len());
        assert_eq!(url("missing.css"), "/");
    }

    #[test]
    fn manifest_icons_are_served() {
        let manifest: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../assets/manifest.webmanifest")).unwrap();
        for icon in manifest["icons"].as_array().unwrap() {
            let name = icon["src"].as_str().unwrap().strip_prefix("/assets/").unwrap();
            assert!(APP_ICONS.iter().any(|asset| asset.name == name), "{name}");
        }
    }

    #[tokio::test]
    async fn icons_are_also_served_by_plain_name() {
        let icon = serve(Path("icon-192.png".to_owned())).await;
        assert_eq!(icon.status(), StatusCode::OK);
        assert_eq!(icon.headers().get(CACHE_CONTROL).unwrap(), "public, max-age=86400");
        let hashed = serve(Path(url("icon-192.png").trim_start_matches("/assets/").to_owned())).await;
        assert_eq!(hashed.status(), StatusCode::OK);
        assert_eq!(serve(Path("app.css".to_owned())).await.status(), StatusCode::NOT_FOUND);
    }
}
