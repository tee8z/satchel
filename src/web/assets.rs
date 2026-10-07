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

/// The hashed URL for a file name such as `app.css`.
pub(crate) fn url(name: &str) -> &'static str {
    ASSETS
        .iter()
        .find(|asset| asset.name == name)
        .map_or("/", |asset| asset.url.as_str())
}

pub(crate) async fn serve(Path(file): Path<String>) -> Response {
    let path = format!("/assets/{file}");
    match ASSETS.iter().find(|asset| asset.url == path) {
        Some(asset) => (
            [
                (CONTENT_TYPE, asset.content_type),
                (CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            asset.body,
        )
            .into_response(),
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
}
