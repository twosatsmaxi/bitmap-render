use std::{io::Write, num::NonZeroUsize, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, Method, Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use lru::LruCache;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing::{error, info};

use common::BlockMeta;

const DEFAULT_CACHE_CAPACITY: usize = 128;

#[derive(Clone)]
struct AppState {
    db: PgPool,
    cache: Arc<Mutex<LruCache<u64, Arc<[u8]>>>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "bitmap_render_backend=info,tower_http=info".to_string()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set");

    let db = PgPool::connect(&database_url)
        .await
        .expect("failed to connect to postgres");
    info!("connected to postgres");

    let cache_capacity = std::env::var("CACHE_CAPACITY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_CACHE_CAPACITY);

    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);

    let state = AppState {
        db,
        cache: Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(cache_capacity).expect("cache capacity must be non-zero"),
        ))),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET]);
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/block/{height}", get(get_block))
        .route("/api/block/{height}/meta", get(get_block_meta))
        .fallback_service(ServeDir::new("frontend/dist"))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind listener");
    info!("listening on 0.0.0.0:{port}");

    axum::serve(listener, app)
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
    if let Some(payload) = state.cache.lock().await.get(&height).cloned() {
        return Ok(binary_response(payload));
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
    let payload = Arc::<[u8]>::from(encoded_bytes);
    state.cache.lock().await.put(height, payload.clone());
    Ok(binary_response(payload))
}

async fn get_block_meta(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    let row: Option<(i32, Option<i64>)> = sqlx::query_as(
        "SELECT tx_count, EXTRACT(EPOCH FROM block_timestamp)::bigint FROM bitmaps WHERE block_height = $1",
    )
    .bind(height as i64)
    .fetch_optional(&state.db)
    .await
    .map_err(AppError::db)?;

    let Some((tx_count, block_timestamp)) = row else {
        return Err(AppError {
            status: StatusCode::NOT_FOUND,
            message: format!("block {height} not found"),
        });
    };

    let timestamp = block_timestamp.unwrap_or(0) as u64;

    Ok(axum::Json(BlockMeta {
        id: String::new(),
        height,
        timestamp,
        size: 0,
        tx_count: tx_count as usize,
    }))
}

fn binary_response(payload: Arc<[u8]>) -> Response<Body> {
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (self.status, self.message).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Matches bitfeed's logTxSize: max(1, ceil(log10(satoshis)) - 5)
    fn log_tx_size(satoshis: u64) -> u8 {
        if satoshis == 0 {
            return 1;
        }
        let log = (satoshis as f64).log10().ceil() as i64;
        (log - 5).max(1) as u8
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
}
