use http::Version;
use poem::{
    error::{IntoResult, NotFound, NotFoundError},
    web::Path,
    Error, IntoResponse, Response, ResponseParts,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../dist"]
#[cfg_eval]
#[cfg_attr(debug_assertions, allow_missing = true)]
pub struct Assets;

#[poem::handler]
pub async fn frontend(path: Option<Path<String>>) -> impl IntoResult<Response> {
    let Path(path) = path.unwrap_or(Path("index.html".to_string()));
    #[cfg(debug_assertions)]
    {
        if let Ok(response) = reqwest::get(format!("http://localhost:5173/{path}")).await
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
    Ok(Response::builder()
        .body(asset.data.to_vec())
        .set_content_type(asset.metadata.mimetype())
        .into_response())
}
