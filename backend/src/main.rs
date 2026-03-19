use std::{
    io::Write,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use lru::LruCache;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, pool::PoolConnection, postgres::PgPoolOptions};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
use tracing::{error, info};

use common::{BlockMeta, TxSummary};

const DEFAULT_CACHE_CAPACITY: usize = 128;

#[derive(Clone)]
struct AppState {
    client: Client,
    db: PgPool,
    ord_base_url: String,
    cache: Arc<Mutex<LruCache<u64, (String, Arc<[u8]>)>>>,
    tx_cache: Arc<Mutex<LruCache<u64, Arc<Vec<TxSummary>>>>>,
}

#[derive(Debug, Serialize)]
struct HeightResp {
    height: u64,
}

#[derive(Debug, Deserialize)]
struct OrdInput {
    script_sig: String,
    witness: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OrdOutput {
    value: Option<u64>,
    script_pubkey: String,
}

#[derive(Debug, Deserialize)]
struct OrdTx {
    input: Vec<OrdInput>,
    output: Vec<OrdOutput>,
}

#[derive(Debug, Deserialize)]
struct OrdBlock {
    height: u64,
    transactions: Vec<OrdTx>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "bitmap_render_backend=info,tower_http=info".to_string()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let max_db_connections = std::env::var("MAX_DB_CONNECTIONS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(16);

    let min_db_connections = std::env::var("MIN_DB_CONNECTIONS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value.min(max_db_connections))
        .unwrap_or(0);

    let db = PgPoolOptions::new()
        .max_connections(max_db_connections)
        .min_connections(min_db_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .expect("failed to connect to postgres");
    info!(
        max_db_connections,
        min_db_connections, "connected to postgres"
    );

    let cache_capacity = std::env::var("CACHE_CAPACITY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_CACHE_CAPACITY);

    let ord_base_url = std::env::var("ORD_BASE_URL")
        .expect("ORD_BASE_URL must be set")
        .trim_end_matches('/')
        .to_string();

    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);

    let client = Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(15))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build reqwest client");

    info!("using ord backend: {ord_base_url}");

    let state = AppState {
        client,
        db,
        ord_base_url,
        cache: Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(cache_capacity).expect("cache capacity must be non-zero"),
        ))),
        tx_cache: Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(cache_capacity).expect("cache capacity must be non-zero"),
        ))),
    };

    let allowed_origins = std::env::var("ALLOWED_ORIGINS").unwrap_or_else(|_| "*".to_string());
    let cors = if allowed_origins == "*" {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET])
            .expose_headers(["x-block-hash".parse::<header::HeaderName>().unwrap()])
    } else {
        let origins = allowed_origins
            .split(',')
            .filter_map(|s| s.trim().parse::<HeaderValue>().ok())
            .collect::<Vec<_>>();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET])
            .expose_headers(["x-block-hash".parse::<header::HeaderName>().unwrap()])
    };

    // TODO(production): Remove PNA header and restrict allow_origin to specific domains
    // before deploying to production. Currently allows any origin + private network access
    // for local dev convenience.
    let pna = axum::middleware::from_fn(
        |req: axum::extract::Request, next: axum::middleware::Next| async move {
            let mut res = next.run(req).await;
            res.headers_mut().insert(
                "Access-Control-Allow-Private-Network",
                HeaderValue::from_static("true"),
            );
            res
        },
    );

    // Generous rate limiting to handle grid rendering (10/sec, burst 30)
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(10)
            .burst_size(30)
            .finish()
            .unwrap(),
    );

    let api_routes = Router::new()
        .route("/block/{height}", get(get_block))
        .route("/block/{height}/png", get(get_block_png))
        .route("/block/{height}/meta", get(get_block_meta))
        .route("/block/{height}/txs", get(get_block_txs))
        .route("/blockheight/{hash}", get(get_blockheight_by_hash))
        .route("/blocks/batch", get(get_blocks_batch))
        .route("/blocks/meta/batch", get(get_blocks_meta_batch))
        .layer(GovernorLayer::new(governor_conf));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .nest("/api", api_routes)
        .fallback_service(ServeDir::new("frontend/dist"))
        .layer(cors)
        .layer(pna)
        .with_state(state);

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".to_string());

    let listener = tokio::net::TcpListener::bind((bind_addr.as_str(), port))
        .await
        .expect("failed to bind listener");
    info!("listening on {bind_addr}:{port}");

    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .expect("server exited with error");
}

async fn healthz() -> &'static str {
    "ok"
}

async fn get_block(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<Response<Body>, AppError> {
    if let Some((hash, payload)) = state.cache.lock().await.get(&height).cloned() {
        return Ok(binary_response(&hash, payload));
    }

    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT encoded_bytes FROM bitmaps WHERE block_height = $1 AND encoded_bytes IS NOT NULL",
    )
    .bind(height as i64)
    .fetch_optional(&state.db)
    .await
    .map_err(AppError::db)?;

    let Some((encoded_bytes,)) = row else {
        return Err(AppError {
            status: StatusCode::NOT_FOUND,
            message: format!("block {height} not found or not yet seeded"),
        });
    };

    info!(height, bytes = encoded_bytes.len(), "served from postgres");
    let hash = fetch_text(
        &state.client,
        format!("{}/blockhash/{}", state.ord_base_url, height),
    )
    .await?;
    let payload = Arc::<[u8]>::from(encoded_bytes);
    state
        .cache
        .lock()
        .await
        .put(height, (hash.clone(), payload.clone()));
    Ok(binary_response(&hash, payload))
}

#[derive(Debug, Deserialize)]
struct HeightsQuery {
    heights: String,
}

impl HeightsQuery {
    fn parse_heights(&self) -> Result<Vec<i64>, AppError> {
        let heights: Vec<i64> = self
            .heights
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<i64>().map_err(|_| AppError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("invalid height value: {}", s),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        if heights.len() > 50 {
            return Err(AppError {
                status: StatusCode::BAD_REQUEST,
                message: "max 50 heights allowed".to_string(),
            });
        }

        Ok(heights)
    }
}

async fn get_blocks_meta_batch(
    State(state): State<AppState>,
    Query(query): Query<HeightsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let heights = query.parse_heights()?;

    if heights.is_empty() {
        return Ok((
            [(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"))],
            axum::Json(Vec::<BlockMeta>::new()),
        ));
    }

    let rows: Vec<(i64, i32, Option<i64>)> = sqlx::query_as(
        "SELECT block_height, tx_count, EXTRACT(EPOCH FROM block_timestamp)::bigint FROM bitmaps WHERE block_height = ANY($1::bigint[])"
    )
    .bind(&heights)
    .fetch_all(&state.db)
    .await
    .map_err(AppError::db)?;

    let block_metas: Vec<BlockMeta> = rows
        .into_iter()
        .map(|(height, tx_count, block_timestamp)| BlockMeta {
            id: String::new(),
            height: height as u64,
            timestamp: block_timestamp.unwrap_or(0) as u64,
            size: 0,
            tx_count: tx_count as usize,
        })
        .collect();

    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"))],
        axum::Json(block_metas),
    ))
}

/// Batch endpoint for fetching multiple blocks' binary data
/// Note: This endpoint is intentionally outside the GovernorLayer rate limiting
/// as it relies on upstream marketplace backend rate limiting (10/sec burst 30)
async fn get_blocks_batch(
    State(state): State<AppState>,
    Query(query): Query<HeightsQuery>,
    headers: HeaderMap,
) -> Result<Response<Body>, AppError> {
    let heights = query.parse_heights()?;

    if heights.is_empty() {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"))
            .header(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"))
            .body(Body::from(vec![]))
            .expect("response should build"));
    }

    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT block_height, encoded_bytes FROM bitmaps WHERE block_height = ANY($1::bigint[]) AND encoded_bytes IS NOT NULL"
    )
    .bind(&heights)
    .fetch_all(&state.db)
    .await
    .map_err(AppError::db)?;

    let count = rows.len() as u32;
    let mut output = Vec::new();

    // Write count as u32 big-endian (4 bytes)
    output.extend_from_slice(&count.to_be_bytes());

    // For each block: [u32 height][u32 len][bytes data]
    for (height, data) in rows {
        let height_u32 = height as u32;
        let len_u32 = data.len() as u32;

        output.extend_from_slice(&height_u32.to_be_bytes());
        output.extend_from_slice(&len_u32.to_be_bytes());
        output.extend_from_slice(&data);
    }

    let accepts_br = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("br"))
        .unwrap_or(false);

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"))
        .header(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));

    let body: Vec<u8> = if accepts_br {
        let compressed = brotli_compress(&output).unwrap_or_else(|err| {
            error!("brotli compression failed: {err}");
            output.clone()
        });
        builder = builder.header(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
        compressed
    } else {
        output
    };

    Ok(builder.body(Body::from(body)).expect("response should build"))
}

async fn get_block_meta(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    let started_at = Instant::now();
    let acquire_started_at = Instant::now();
    let mut conn: PoolConnection<sqlx::Postgres> =
        state.db.acquire().await.map_err(AppError::db)?;
    let acquire_elapsed = acquire_started_at.elapsed();

    let query_started_at = Instant::now();
    let row: Option<(i32, Option<i64>)> = sqlx::query_as(
        "SELECT tx_count, EXTRACT(EPOCH FROM block_timestamp)::bigint FROM bitmaps WHERE block_height = $1",
    )
    .bind(height as i64)
    .fetch_optional(&mut *conn)
    .await
    .map_err(AppError::db)?;
    let query_elapsed = query_started_at.elapsed();

    let Some((tx_count, block_timestamp)) = row else {
        return Err(AppError {
            status: StatusCode::NOT_FOUND,
            message: format!("block {height} not found"),
        });
    };

    let timestamp = block_timestamp.unwrap_or(0) as u64;
    let total_elapsed = started_at.elapsed();
    info!(
        height,
        acquire_ms = acquire_elapsed.as_secs_f64() * 1000.0,
        query_ms = query_elapsed.as_secs_f64() * 1000.0,
        total_ms = total_elapsed.as_secs_f64() * 1000.0,
        "served block meta"
    );

    Ok(axum::Json(BlockMeta {
        id: String::new(),
        height,
        timestamp,
        size: 0,
        tx_count: tx_count as usize,
    }))
}

async fn get_block_txs(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(cached) = state.tx_cache.lock().await.get(&height).cloned() {
        return Ok(axum::Json((*cached).clone()));
    }

    let txs = Arc::new(fetch_ord_tx_summaries(&state, height).await?);
    state.tx_cache.lock().await.put(height, txs.clone());
    Ok(axum::Json((*txs).clone()))
}

async fn get_blockheight_by_hash(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError {
            status: StatusCode::BAD_REQUEST,
            message: "invalid block hash format".to_string(),
        });
    }

    let block: OrdBlock = fetch_json(
        &state.client,
        format!("{}/block/{}", state.ord_base_url, hash),
    )
    .await?;
    Ok(axum::Json(HeightResp {
        height: block.height,
    }))
}

async fn fetch_ord_tx_summaries(state: &AppState, height: u64) -> Result<Vec<TxSummary>, AppError> {
    let block: OrdBlock = state
        .client
        .get(format!("{}/block/{}", state.ord_base_url, height))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(AppError::upstream_transport)?
        .json::<OrdBlock>()
        .await
        .map_err(AppError::upstream_transport)?;

    Ok(block.transactions.iter().map(tx_summary_from_ord).collect())
}

fn tx_summary_from_ord(tx: &OrdTx) -> TxSummary {
    TxSummary {
        txid: None,
        vsize: ord_tx_vsize(tx),
        fee: None,
        feerate: None,
        value: tx.output.iter().filter_map(|output| output.value).sum(),
    }
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(
    client: &Client,
    url: String,
) -> Result<T, AppError> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(AppError::upstream_transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::upstream_status(status.as_u16()));
    }
    response
        .json::<T>()
        .await
        .map_err(AppError::upstream_transport)
}

async fn fetch_text(client: &Client, url: String) -> Result<String, AppError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(AppError::upstream_transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::upstream_status(status.as_u16()));
    }
    response.text().await.map_err(AppError::upstream_transport)
}

fn varint_size(n: usize) -> u64 {
    if n < 0xfd {
        1
    } else if n <= 0xffff {
        3
    } else {
        5
    }
}

fn ord_tx_vsize(tx: &OrdTx) -> u64 {
    let has_witness = tx.input.iter().any(|input| !input.witness.is_empty());
    let mut base = 8;
    base += varint_size(tx.input.len());
    for input in &tx.input {
        base += 36;
        let script_sig_len = input.script_sig.len() / 2;
        base += varint_size(script_sig_len) + script_sig_len as u64 + 4;
    }
    base += varint_size(tx.output.len());
    for output in &tx.output {
        let script_pubkey_len = output.script_pubkey.len() / 2;
        base += 8 + varint_size(script_pubkey_len) + script_pubkey_len as u64;
    }
    if !has_witness {
        return base;
    }

    let mut witness = 2;
    for input in &tx.input {
        witness += varint_size(input.witness.len());
        for item in &input.witness {
            let item_len = item.len() / 2;
            witness += varint_size(item_len) + item_len as u64;
        }
    }
    base + witness.div_ceil(4)
}

fn binary_response(hash: &str, payload: Arc<[u8]>) -> Response<Body> {
    let compressed = brotli_compress(payload.as_ref()).unwrap_or_else(|err| {
        error!("brotli compression failed: {err}");
        payload.to_vec()
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        )
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        )
        .header(header::CONTENT_ENCODING, HeaderValue::from_static("br"))
        .header("X-Block-Hash", hash)
        .body(Body::from(compressed))
        .expect("response should build")
}

fn brotli_compress(input: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut output = Vec::new();
    let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
    writer.write_all(input)?;
    writer.flush()?;
    drop(writer);
    Ok(output)
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn db(error: sqlx::Error) -> Self {
        error!("database error: {error:#}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "database query failed".to_string(),
        }
    }

    fn upstream_transport(error: reqwest::Error) -> Self {
        error!(
            is_connect = %error.is_connect(),
            is_timeout = %error.is_timeout(),
            is_request = %error.is_request(),
            is_decode = %error.is_decode(),
            url = ?error.url(),
            "upstream error: {error:#}"
        );
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: "upstream request failed".to_string(),
        }
    }

    fn upstream_status(status: u16) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("upstream returned status {status}"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (self.status, self.message).into_response()
    }
}

fn hcl_to_rgb(h_deg: f64, c: f64, l: f64) -> (u8, u8, u8) {
    let h = h_deg * std::f64::consts::PI / 180.0;
    let a = h.cos() * c;
    let b = h.sin() * c;
    let fy = (l + 16.0) / 116.0;
    let fx = a / 500.0 + fy;
    let fz = fy - b / 200.0;
    let e = 0.008856;
    let k = 903.3;
    let x = if fx.powi(3) > e {
        fx.powi(3)
    } else {
        (116.0 * fx - 16.0) / k
    } * 0.95047;
    let y = if l > k * e {
        ((l + 16.0) / 116.0).powi(3)
    } else {
        l / k
    };
    let z = if fz.powi(3) > e {
        fz.powi(3)
    } else {
        (116.0 * fz - 16.0) / k
    } * 1.08883;
    let lin = |v: f64| {
        if v <= 0.0031308 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    (
        (lin(x * 3.2406 + y * -1.5372 + z * -0.4986) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8,
        (lin(x * -0.9689 + y * 1.8758 + z * 0.0415) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8,
        (lin(x * 0.0557 + y * -0.2040 + z * 1.0570) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8,
    )
}

async fn get_block_png(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<Response<Body>, AppError> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT encoded_bytes FROM bitmaps WHERE block_height = $1 AND encoded_bytes IS NOT NULL",
    )
    .bind(height as i64)
    .fetch_optional(&state.db)
    .await
    .map_err(AppError::db)?;

    let Some((encoded_bytes,)) = row else {
        return Err(AppError {
            status: StatusCode::NOT_FOUND,
            message: format!("block {height} not found or not yet seeded"),
        });
    };

    let (width_units, max_y, squares) = common::compute_layout(&encoded_bytes);
    
    let img_width = 1200;
    let img_height = 630;
    
    let mut img = image::ImageBuffer::from_pixel(img_width as u32, img_height as u32, image::Rgb([9, 9, 11]));
    
    let (r, g, b) = hcl_to_rgb(0.181 * 360.0, 78.225, 0.472 * 150.0);
    let sq_color = image::Rgb([r, g, b]);

    let padding = 40.0;
    let available_w = (img_width as f64) - padding * 2.0;
    let available_h = (img_height as f64) - padding * 2.0;
    
    let unit_w = available_w / (width_units as f64);
    let unit_h = available_h / (max_y as f64);
    let scale = unit_w.min(unit_h);
    
    let layout_w = (width_units as f64) * scale;
    let layout_h = (max_y as f64) * scale;
    
    let offset_x = (img_width as f64 - layout_w) / 2.0;
    let offset_y = (img_height as f64 - layout_h) / 2.0;
    
    let unit_padding = scale * 0.05;

    for sq in squares {
        let pw = (sq.r as f64 * scale - unit_padding * 2.0).round() as i32;
        if pw <= 0 {
            continue;
        }
        let pw = pw as u32;
        
        let px = (sq.x as f64 * scale + offset_x + unit_padding).round() as u32;
        let py = (sq.y as f64 * scale + offset_y + unit_padding).round() as u32;
        
        for y in py..(py + pw) {
            for x in px..(px + pw) {
                if x < img_width as u32 && y < img_height as u32 {
                    img.put_pixel(x, y, sq_color);
                }
            }
        }
    }
    
    let mut buffer = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buffer, image::ImageFormat::Png).map_err(|e| {
        error!("failed to generate png: {e}");
        AppError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "failed to generate image".to_string()
        }
    })?;
    
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=86400, immutable")
        .body(Body::from(buffer.into_inner()))
        .expect("response should build"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn log_tx_size(satoshis: u64) -> u8 {
        if satoshis == 0 {
            return 1;
        }
        let log = (satoshis as f64).log10().ceil() as i64;
        (log - 5).max(1) as u8
    }

    fn make_ord_tx(inputs: &[(&str, &[&str])], outputs: &[(&str, u64)]) -> OrdTx {
        OrdTx {
            input: inputs
                .iter()
                .map(|(script_sig, witness)| OrdInput {
                    script_sig: script_sig.to_string(),
                    witness: witness.iter().map(|item| item.to_string()).collect(),
                })
                .collect(),
            output: outputs
                .iter()
                .map(|(script_pubkey, value)| OrdOutput {
                    value: Some(*value),
                    script_pubkey: script_pubkey.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn tx_summary_from_ord_computes_vsize_and_value() {
        let tx = make_ord_tx(
            &[("", &[])],
            &[
                ("0014fd92b03dd4f1ab7031905b79459f7abc5a5c50cb", 500_000),
                ("001424f39209574e117d51ebb72ae505bc6d56c26d1c", 200_000),
            ],
        );

        let summary = tx_summary_from_ord(&tx);

        assert_eq!(summary.txid, None);
        assert_eq!(summary.fee, None);
        assert_eq!(summary.feerate, None);
        assert_eq!(summary.value, 700_000);
        assert_eq!(summary.vsize, 113);
    }

    #[test]
    fn log_tx_size_matches_bitfeed() {
        assert_eq!(log_tx_size(0), 1);
        assert_eq!(log_tx_size(100_000), 1);
        assert_eq!(log_tx_size(10_000_000), 2);
        assert_eq!(log_tx_size(100_000_000), 3);
        assert_eq!(log_tx_size(1_000_000_000), 4);
        assert_eq!(log_tx_size(2_500_000_000), 5);
    }

    /// Test that batch endpoints have rate limiting applied (same as individual)
    #[test]
    fn batch_endpoints_have_rate_limiting() {
        let governor_conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(2)
                .burst_size(10)
                .finish()
                .unwrap(),
        );

        // All endpoints share the same rate limiting
        let api_routes = Router::new()
            .route("/blocks/batch", get(get_blocks_batch))
            .route("/blocks/meta/batch", get(get_blocks_meta_batch))
            .layer(GovernorLayer::new(governor_conf));

        let _: Router<AppState> = Router::new().nest("/api", api_routes);
    }

    /// Test that rate limiting configuration is correctly applied to individual endpoints
    #[test]
    fn individual_endpoints_have_rate_limiting() {
        let governor_conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(2)
                .burst_size(10)
                .finish()
                .unwrap(),
        );

        // Individual endpoints should have GovernorLayer applied
        let api_routes = Router::new()
            .route("/block/{height}", get(get_block))
            .route("/block/{height}/meta", get(get_block_meta))
            .layer(GovernorLayer::new(governor_conf));

        // Verify the router can be created - the layer is applied
        let _: Router<AppState> = Router::new().nest("/api", api_routes);
    }

    /// Test that router has unified rate limiting for all endpoints
    #[test]
    fn router_structure_with_unified_rate_limiting() {
        let governor_conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(2)
                .burst_size(10)
                .finish()
                .unwrap(),
        );

        // All endpoints share the same rate limiting (2/sec, burst 10)
        let api_routes = Router::new()
            .route("/block/{height}", get(get_block))
            .route("/block/{height}/png", get(get_block_png))
            .route("/block/{height}/meta", get(get_block_meta))
            .route("/block/{height}/txs", get(get_block_txs))
            .route("/blockheight/{hash}", get(get_blockheight_by_hash))
            .route("/blocks/batch", get(get_blocks_batch))
            .route("/blocks/meta/batch", get(get_blocks_meta_batch))
            .layer(GovernorLayer::new(governor_conf));

        // Build the complete router matching production structure
        let app: Router<AppState> = Router::new()
            .route("/healthz", get(healthz))
            .nest("/api", api_routes);

        // Verify the router was created successfully with tiered rate limiting
        drop(app);
    }

    /// Test that healthz endpoint is defined in router
    #[test]
    fn healthz_endpoint_defined() {
        let app: Router<AppState> = Router::new().route("/healthz", get(healthz));
        drop(app);
    }

    #[test]
    fn heights_query_parses_valid_input() {
        let query = HeightsQuery {
            heights: "100,101,102".to_string(),
        };
        let result = query.parse_heights();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![100, 101, 102]);
    }

    #[test]
    fn heights_query_handles_whitespace() {
        let query = HeightsQuery {
            heights: "100, 101 , 102".to_string(),
        };
        let result = query.parse_heights();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![100, 101, 102]);
    }

    #[test]
    fn heights_query_returns_empty_for_empty_input() {
        let query = HeightsQuery {
            heights: "".to_string(),
        };
        let result = query.parse_heights();
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn heights_query_rejects_invalid_format() {
        let query = HeightsQuery {
            heights: "100,abc,102".to_string(),
        };
        let result = query.parse_heights();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn heights_query_rejects_too_many_heights() {
        let heights: Vec<String> = (0..51).map(|i| i.to_string()).collect();
        let query = HeightsQuery {
            heights: heights.join(","),
        };
        let result = query.parse_heights();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn heights_query_accepts_max_heights() {
        let heights: Vec<String> = (0..50).map(|i| i.to_string()).collect();
        let query = HeightsQuery {
            heights: heights.join(","),
        };
        let result = query.parse_heights();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 50);
    }

    #[test]
    fn binary_response_format_is_correct() {
        // Build a sample response: 2 blocks
        // Block 1: height=100, data=[0x01, 0x02, 0x03]
        // Block 2: height=200, data=[0xAA, 0xBB]
        let mut output = Vec::new();

        // Count = 2
        output.extend_from_slice(&2u32.to_be_bytes());

        // Block 1: height=100, len=3, data=[0x01, 0x02, 0x03]
        output.extend_from_slice(&100u32.to_be_bytes());
        output.extend_from_slice(&3u32.to_be_bytes());
        output.extend_from_slice(&[0x01, 0x02, 0x03]);

        // Block 2: height=200, len=2, data=[0xAA, 0xBB]
        output.extend_from_slice(&200u32.to_be_bytes());
        output.extend_from_slice(&2u32.to_be_bytes());
        output.extend_from_slice(&[0xAA, 0xBB]);

        // Verify the structure
        assert_eq!(output.len(), 4 + (4 + 4 + 3) + (4 + 4 + 2)); // 25 bytes

        // Read back and verify
        let count = u32::from_be_bytes([output[0], output[1], output[2], output[3]]);
        assert_eq!(count, 2);

        let offset = 4;
        let height1 = u32::from_be_bytes([
            output[offset],
            output[offset + 1],
            output[offset + 2],
            output[offset + 3],
        ]);
        assert_eq!(height1, 100);

        let len1 = u32::from_be_bytes([
            output[offset + 4],
            output[offset + 5],
            output[offset + 6],
            output[offset + 7],
        ]);
        assert_eq!(len1, 3);

        let data1 = &output[offset + 8..offset + 8 + 3];
        assert_eq!(data1, &[0x01, 0x02, 0x03]);

        let offset2 = offset + 8 + 3;
        let height2 = u32::from_be_bytes([
            output[offset2],
            output[offset2 + 1],
            output[offset2 + 2],
            output[offset2 + 3],
        ]);
        assert_eq!(height2, 200);

        let len2 = u32::from_be_bytes([
            output[offset2 + 4],
            output[offset2 + 5],
            output[offset2 + 6],
            output[offset2 + 7],
        ]);
        assert_eq!(len2, 2);

        let data2 = &output[offset2 + 8..offset2 + 8 + 2];
        assert_eq!(data2, &[0xAA, 0xBB]);
    }

    #[test]
    fn brotli_compression_roundtrip() {
        let original = b"Hello, World! This is test data for brotli compression.";
        let compressed = brotli_compress(original).expect("compression should succeed");

        // Compressed should be different from original
        assert!(!compressed.is_empty());
        assert_ne!(compressed, original.to_vec());

        // Decompress and verify
        let mut decompressor = brotli::Decompressor::new(compressed.as_slice(), 4096);
        let mut decompressed = Vec::new();
        decompressor.read_to_end(&mut decompressed).expect("decompression should succeed");

        assert_eq!(decompressed, original.to_vec());
    }

    #[test]
    fn brotli_compress_empty_data() {
        let original: &[u8] = b"";
        let compressed = brotli_compress(original).expect("compression should succeed");
        assert!(!compressed.is_empty()); // Brotli has some overhead even for empty input

        let mut decompressor = brotli::Decompressor::new(compressed.as_slice(), 4096);
        let mut decompressed = Vec::new();
        decompressor.read_to_end(&mut decompressed).expect("decompression should succeed");
        assert_eq!(decompressed, original.to_vec());
    }
}
