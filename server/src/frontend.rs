use anyhow::Result;
#[cfg(debug_assertions)]
use http::Version;
use poem::{
    Error, IntoResponse, Request, Response,
    error::{IntoResult, NotFoundError},
    web::Path,
};
#[cfg(debug_assertions)]
use poem::{ResponseParts, error::NotFound};
use rust_embed::RustEmbed;
use serde_json::json;
#[cfg(debug_assertions)]
use std::time::Duration;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

/// Inclusive size bounds (in pixels) of the rasterised icon endpoints served
/// from the brand vector, e.g. `/icon-192.png`.
const ICON_SIZES: std::ops::RangeInclusive<u32> = 32..=2048;

/// The brand icon, embedded directly so it can be rasterised on demand even
/// when no built frontend bundle is present.
const BRAND_SVG: &[u8] = include_bytes!("../../webui/public/webshooter.svg");

/// Rasterised icon cache keyed by pixel size.
static ICON_CACHE: LazyLock<Mutex<HashMap<u32, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(RustEmbed)]
#[folder = "../dist"]
#[cfg_eval]
#[cfg_attr(debug_assertions, allow_missing = true)]
pub struct Assets;

#[cfg(debug_assertions)]
fn vite_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

/// The base path the application is served under, derived from the request.
///
/// The first recognised proxy header wins, since a reverse proxy that strips a
/// prefix from the path must advertise it explicitly (`X-Forwarded-Prefix` /
/// `X-Forwarded-Path` / RFC 7239 `Forwarded: path=`).  When none are present
/// the application is deemed to be served at the origin root.
fn base_path(req: &Request) -> String {
    let raw = req
        .headers()
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            req.headers()
                .get("x-forwarded-path")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
        })
        .or_else(|| {
            req.headers()
                .get("forwarded")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    v.split(';')
                        .find_map(|part| part.trim().strip_prefix("path="))
                        .map(|p| p.trim_matches('"'))
                })
        })
        .unwrap_or("/");

    let mut base = raw.trim().to_owned();
    if !base.starts_with('/') {
        base = format!("/{base}");
    }
    while base.len() > 1 && base.ends_with('/') {
        base.pop();
    }
    base
}

/// Generate the PWA manifest with a base-relative `start_url`/`scope`.
///
/// The static manifest (embedded with the rest of the frontend) deliberately
/// omits `start_url`/`scope`; they are derived from the request's base path so
/// the app can be served from any mount point behind a proxy and the web
/// manifest still points at the installation/base of *that* installation.
#[poem::handler]
pub async fn frontend(req: &Request, path: Option<Path<String>>) -> impl IntoResult<Response> {
    let Path(path) = path.unwrap_or(Path("index.html".to_string()));

    // The web manifest is always generated at the request's base path: its
    // `start_url`/`scope` depend on how the app is mounted behind a proxy.
    if path == "manifest.webmanifest" {
        return Ok::<_, Error>(manifest_response(req).into_response());
    }

    // Rasterised icon endpoints; anything that doesn't match this shape falls
    // through to the regular asset pipeline.
    if let Some(size) = icon_size_from_path(&path) {
        return Ok::<_, Error>(icon_response(size).into_response());
    }

    #[cfg(debug_assertions)]
    {
        if let Ok(response) = vite_client()
            .get(format!("http://localhost:5173/{path}"))
            .send()
            .await
            && response.status().is_success()
        {
            let parts = ResponseParts {
                status: response.status(),
                version: Version::default(),
                headers: response.headers().to_owned(),
                extensions: response.extensions().to_owned(),
            };
            let body = response.bytes().await.map_err(NotFound)?.to_vec();
            return Ok::<_, Error>(Response::from_parts(parts, body.into()));
        }
    }
    let asset = Assets::get(&path).ok_or(NotFoundError)?;
    Ok::<_, Error>(
        Response::builder()
            .body(asset.data.to_vec())
            .set_content_type(asset.metadata.mimetype())
            .into_response(),
    )
}

/// Match `/icon-{size}.png` within ICON_SIZES range.
fn icon_size_from_path(path: &str) -> Option<u32> {
    let size = path
        .strip_prefix("icon-")?
        .strip_suffix(".png")?
        .parse::<u32>()
        .ok()?;
    ICON_SIZES.contains(&size).then_some(size)
}

/// Rasterise [`BRAND_SVG`] to a square `size`x`size` PNG, cached per size.
fn rendered_icon(size: u32) -> Result<Vec<u8>> {
    if let Some(cached) = ICON_CACHE.lock().unwrap().get(&size) {
        return Ok(cached.clone());
    }
    let svg = std::str::from_utf8(BRAND_SVG)?;
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())?;
    let scale = size as f32 / tree.size().width();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)
        .ok_or_else(|| anyhow::anyhow!("could not allocate a {size}x{size} pixmap"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let png = pixmap.encode_png()?;
    ICON_CACHE.lock().unwrap().insert(size, png.clone());
    Ok(png)
}

fn icon_response(size: u32) -> Response {
    match rendered_icon(size) {
        Ok(body) => Response::builder().content_type("image/png").body(body),
        Err(err) => {
            log::error!("failed to rasterise icon-{size}.png: {err:#}");
            Response::builder()
                .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                .finish()
        }
    }
}

/// A generated web manifest for the current base path.
fn manifest_response(req: &Request) -> Response {
    let asset = Assets::get("manifest.webmanifest");
    let base = match asset {
        None => json!({}),
        Some(asset) => serde_json::from_slice(&asset.data).unwrap_or_else(|_| json!({})),
    };
    let mut manifest = match base {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    let base = base_path(req);
    manifest.insert("start_url".into(), json!(format!("{base}/")));
    manifest.insert("scope".into(), json!(format!("{base}/")));

    Response::builder()
        .content_type("application/manifest+json")
        .body(serde_json::to_vec(&manifest).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::{icon_size_from_path, rendered_icon};

    #[test]
    fn icon_path_parsing() {
        assert_eq!(icon_size_from_path("icon-192.png"), Some(192));
        assert_eq!(icon_size_from_path("icon-512.png"), Some(512));
        assert_eq!(icon_size_from_path("icon-32.png"), Some(32));
        assert_eq!(icon_size_from_path("icon-2048.png"), Some(2048));
        assert_eq!(icon_size_from_path("icon-0.png"), None);
        assert_eq!(icon_size_from_path("icon-abc.png"), None);
        assert_eq!(icon_size_from_path("webshooter.svg"), None);
        assert_eq!(icon_size_from_path("icon-192.png/extra"), None);
    }

    #[test]
    fn brand_svg_rasterises() {
        for size in [192, 512] {
            let png = rendered_icon(size).unwrap();
            assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        }
    }
}
