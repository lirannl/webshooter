use futures_util::Future;
use warp::{
    reject::Rejection,
    reply::{self, Reply},
};

/// Easily use a fallible async anyhow in an and_then - provide an async move {} block
pub async fn handle(
    handler: impl Future<
        Output = Result<warp::http::Response<warp::hyper::Body>, Box<dyn std::error::Error>>,
    >,
) -> Result<warp::http::Response<warp::hyper::Body>, Rejection> {
    let result = handler.await;
    result.or_else(|err| {
        eprintln!("HTTP Error: {err:#?}");
        Ok::<_, Rejection>(
            reply::with_status(
                err.to_string(),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response(),
        )
    })
}
