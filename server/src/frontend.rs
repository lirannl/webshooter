use anyhow::{bail, Result};
use bytes::Bytes;
use futures_util::{future::join, SinkExt, StreamExt, TryStreamExt};
use http::{response, Request, Response, StatusCode};
use lazy_static::lazy_static;
use regex::Regex;
use reqwest::Method;
use rust_embed::RustEmbed;
use std::str::FromStr;
use tokio_tungstenite::connect_async;
use warp::{
    filters::{body, method, path},
    path::FullPath,
    reject::reject,
    reply::Reply,
    Filter, Rejection,
};

#[derive(RustEmbed)]
#[folder = "../dist"]
pub struct Assets;

lazy_static! {
    #[cfg(debug_assertions)]
    static ref PROXY_ADDR: &'static str = "http://localhost:5173";
    #[cfg(debug_assertions)]
    static ref PROXY_ADDR_WS: String = Regex::new("^http(s?://)")
            .unwrap()
            .replace(&PROXY_ADDR, "ws$1").to_string();
}

pub fn setup_frontend() -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    let frontend = warp_embed::embed(&Assets);

    #[cfg(debug_assertions)]
    let proxy_ws = path::full()
        .and(warp::ws())
        .map(|path: FullPath, ws: warp::ws::Ws| {
            ws.on_upgrade(|websocket| async {
                let (mut req_tx, mut req_rx) = websocket.split();
                let _ = (async move || -> Result<()> {
                    let (devserver, response) =
                        connect_async(PROXY_ADDR_WS.to_string() + path.as_str()).await?;
                    if !response.status().is_success() {
                        bail!(
                            "Couldn't connect to websocket: {:#?}",
                            response
                                .into_body()
                                .and_then(|b| String::from_utf8(b).ok())
                                .unwrap_or_default()
                        )
                    }
                    let (mut tx, mut rx) = devserver.split();
                    let result: (anyhow::Result<()>, anyhow::Result<()>) = join(
                        async {
                            while let Ok(Some(message)) = req_rx.try_next().await
                                && !message.is_close()
                            {
                                tx.send(tokio_tungstenite::tungstenite::Message::Binary(
                                    message.into_bytes(),
                                ))
                                .await?;
                            }
                            Ok(())
                        },
                        async {
                            while let Ok(Some(message)) = rx.try_next().await
                                && !message.is_close()
                            {
                                req_tx
                                    .send(warp::ws::Message::binary(message.into_data()))
                                    .await?;
                            }
                            Ok(())
                        },
                    )
                    .await;
                    result.0?;
                    result.1?;
                    //devserver.chain(websocket);
                    // devserver.chain(websocket.take_while(|msg| !msg.ok()?.is_close()))
                    Ok(())
                })()
                .await;
            })
        });

    #[cfg(debug_assertions)]
    let frontend = proxy_ws
        .or(method::method()
            .and(path::full())
            .and(warp::header::headers_cloned())
            .and(body::bytes())
            .and_then(
                |method: warp::http::Method,
                 path: FullPath,
                 headers: warp::http::HeaderMap,
                 body: Bytes| async {
                    (|| async move {
                        let response = reqwest::Client::new()
                            .request(
                                Method::from_str(method.as_str())?,
                                PROXY_ADDR.to_string() + path.as_str(),
                            )
                            .headers(reqwest::header::HeaderMap::from_iter(
                                headers.into_iter().filter_map(|(name, value)| {
                                    Some((
                                        reqwest::header::HeaderName::from_str(name?.as_str())
                                            .ok()?,
                                        reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                                            .ok()?,
                                    ))
                                }),
                            ))
                            .body(body.to_vec())
                            .send()
                            .await?;

                        let mut new_response = warp::http::response::Builder::new().status(
                            warp::http::StatusCode::from_str(response.status().as_str())?,
                        );
                        for (name, value) in response.headers() {
                            new_response = new_response.header(name.as_str(), value.as_bytes());
                        }
                        let new_response = new_response.body(response.bytes().await?.to_vec());
                        Ok(new_response)
                    })()
                    .await
                    .map_err(|_err: anyhow::Error| reject())
                },
            ))
        .or(frontend);
    frontend
}

static DEVSERVER: &'static str = "http://localhost:5173";

pub async fn serve_frontend(
    request: &Request<()>,
    body: Option<Vec<u8>>,
) -> Result<(Response<()>, Vec<u8>)> {
    #[cfg(debug_assertions)]
    {
        let httpclient = reqwest::Client::new();
        let health_check = httpclient
            .get(DEVSERVER.to_string())
            .send()
            .await
            .and_then(|res| Ok(res.error_for_status()));
        if health_check.is_ok() {
            let mut proxy_request = httpclient.request(
                request.method().clone(),
                DEVSERVER.to_string()
                    + request
                        .uri()
                        .path_and_query()
                        .map(|pq| pq.as_str())
                        .unwrap_or_default(),
            );
            proxy_request = proxy_request.headers(request.headers().clone());
            if let Some(body) = body {
                proxy_request = proxy_request.body(body);
            }
            let proxy_response = proxy_request.send().await?;

            let mut response = Response::builder();
            for (key, value) in proxy_response.headers() {
                response = response.header(key, value);
            }
            return Ok((response.body(())?, proxy_response.bytes().await?.to_vec()));
        }
    }
    if request.method() != Method::GET {
        return Ok((
            Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(())?,
            vec![],
        ));
    } else if let Some(asset) = Assets::get(request.uri().path()) {
        let response = response::Builder::new()
            .header("Content-Type", asset.metadata.mimetype())
            .body(())?;
        return Ok((response, asset.data.to_vec()));
    }
    Ok((
        response::Builder::new()
            .status(StatusCode::NOT_FOUND)
            .body(())?,
        vec![],
    ))
}
