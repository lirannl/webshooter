#[cfg(debug_assertions)]
use salvo::http::HeaderMap;
use salvo::{Depot, FlowCtrl, Handler, Request, Response, handler};

#[cfg(not(debug_assertions))]
mod embedded {
    use rust_embed::Embed;
    #[derive(Embed)]
    #[folder = "../dist"]
    pub struct Assets;
}

/// Serves the built web UI. In release builds the files are embedded in the
/// binary via `rust-embed`; in debug builds they are read from the `dist/`
/// directory on disk, with the Vite dev server on `localhost:5173` tried first
/// so the UI can be edited live without rebuilding.
#[cfg(debug_assertions)]
#[handler]
pub async fn index(req: &mut Request, res: &mut Response) {
    let path = req.uri().path().to_string();
    if let Ok(upstream) = reqwest::get(format!("http://localhost:5173{path}")).await
        && upstream.status().is_success()
    {
        res.status_code(upstream.status());
        copy_headers(upstream.headers(), res.headers_mut());
        let body = upstream.bytes().await.unwrap_or_default();
        res.body(body.to_vec());
        return;
    }
    let dir = salvo::serve_static::StaticDir::new(["dist"]).defaults("index.html");
    Handler::handle(&dir, req, &mut Depot::new(), res, &mut FlowCtrl::new(Vec::new())).await;
}

#[cfg(not(debug_assertions))]
#[handler]
pub async fn index(req: &mut Request, res: &mut Response) {
    use salvo::serve_static::static_embed;
    let handler = static_embed::<embedded::Assets>().defaults(["index.html"]);
    Handler::handle(&handler, req, &mut Depot::new(), res, &mut FlowCtrl::new(Vec::new())).await;
}

#[cfg(debug_assertions)]
fn copy_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from.iter() {
        to.insert(name, value.clone());
    }
}
