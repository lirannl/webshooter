use anyhow::Result;
#[cfg(debug_assertions)]
use http::Version;
use poem::{
    Error, IntoResponse, Request, Response,
    error::{IntoResult, NotFoundError, ResponseError},
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

/// Rasterised icon cache keyed by pixel size.
static ICON_CACHE: LazyLock<Mutex<HashMap<u32, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn clear_icons_raster_cache() {
    if let Ok(mut icon_cache) = ICON_CACHE.lock() {
        icon_cache.clear();
    }
}

#[derive(RustEmbed)]
#[folder = "../dist"]
#[cfg_eval]
#[cfg_attr(debug_assertions, allow_missing = true)]
pub struct Assets;

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

// The dev server Vite runs on; used only in debug builds.
#[cfg(debug_assertions)]
const VITE_DEV_SERVER: &str = "http://localhost:5173";

/// Resolve a frontend asset through the same sources as the static route: the
/// Vite dev server when a built bundle isn't embedded, then the embedded
/// bundle.
async fn asset_bytes(path: &str) -> Option<Vec<u8>> {
    #[cfg(debug_assertions)]
    {
        if let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            && let Ok(response) = client.get(format!("{VITE_DEV_SERVER}/{path}")).send().await
            && response.status().is_success()
        {
            return response.bytes().await.ok().map(|bytes| bytes.to_vec());
        }
    }
    Assets::get(path).map(|asset| asset.data.to_vec())
}

/// Serve the web frontend, generating paths that depend on the mount point.
#[poem::handler]
pub async fn frontend(req: &Request, path: Option<Path<String>>) -> impl IntoResult<Response> {
    let Path(path) = path.unwrap_or(Path("index.html".to_string()));

    // The web manifest is always generated at the request's base path: its
    // `start_url`/`scope` depend on how the app is mounted behind a proxy.
    if path == "manifest.webmanifest" {
        return Ok::<_, Error>(
            manifest_response(req, asset_bytes("manifest.webmanifest").await).into_response(),
        );
    }

    // Rasterised icon endpoints; anything that doesn't match this shape falls
    // through to the regular asset pipeline.
    if let Some(size) = icon_size_from_path(&path) {
        let Some(svg) = asset_bytes("webshooter.svg").await else {
            return Ok::<_, Error>(NotFoundError.as_response());
        };
        return Ok::<_, Error>(icon_response(size, &svg).into_response());
    }

    #[cfg(debug_assertions)]
    {
        if let Ok(response) = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(NotFound)?
            .get(format!("{VITE_DEV_SERVER}/{path}"))
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

/// Rasterise the brand SVG to a square `size`x`size` PNG, cached per size.
fn rendered_icon(size: u32, svg: &[u8]) -> Result<Vec<u8>> {
    if let Ok(cache) = ICON_CACHE.lock()
        && let Some(cached) = cache.get(&size).cloned()
    {
        return Ok(cached);
    }
    let svg = std::str::from_utf8(svg)?;
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
    if let Ok(mut icon_cache) = ICON_CACHE.lock() {
        icon_cache.insert(size, png.clone());
    }
    Ok(png)
}

fn icon_response(size: u32, svg: &[u8]) -> Response {
    match rendered_icon(size, svg) {
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
///
/// The underlying manifest is resolved like any other frontend asset, with
/// `start_url`/`scope` overlaid on top of it from the request's base path, so
/// the app can be served from any mount point behind a proxy and the web
/// manifest still points at the installation/base of *that* installation.
fn manifest_response(req: &Request, base_manifest: Option<Vec<u8>>) -> Response {
    let base = match base_manifest {
        Some(data) => serde_json::from_slice(&data).unwrap_or_else(|_| json!({})),
        None => json!({}),
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

    const BRAND_SVG: &[u8] = include_bytes!("../../webui/public/webshooter.svg");

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
            let png = rendered_icon(size, BRAND_SVG).unwrap();
            assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        }
    }
}
