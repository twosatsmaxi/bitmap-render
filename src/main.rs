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
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};

const DEFAULT_MEMPOOL_BASE_URL: &str = "https://mempool.space/api";
const DEFAULT_CACHE_CAPACITY: usize = 128;
const PAGE_SIZE: usize = 25;

#[derive(Clone)]
struct AppState {
    client: Client,
    mempool_base_url: String,
    cache: Arc<Mutex<LruCache<u64, Arc<[u8]>>>>,
}

#[derive(Debug, Deserialize)]
struct Block {
    id: String,
    height: u64,
    timestamp: u64,
    size: u64,
    tx_count: usize,
}

#[derive(Debug, Deserialize)]
struct TxOut {
    value: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Tx {
    txid: String,
    vout: Option<Vec<TxOut>>,
}

#[derive(Debug, serde::Serialize)]
struct BlockMeta {
    id: String,
    height: u64,
    timestamp: u64,
    size: u64,
    tx_count: usize,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "bitmap_render_backend=info,tower_http=info".to_string()),
        )
        .init();

    let cache_capacity = std::env::var("CACHE_CAPACITY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_CACHE_CAPACITY);

    let mempool_base_url = std::env::var("MEMPOOL_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_MEMPOOL_BASE_URL.to_string())
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
        .build()
        .expect("failed to build reqwest client");

    let state = AppState {
        client,
        mempool_base_url,
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
    if let Some(cached) = state.cache.lock().await.get(&height).cloned() {
        return Ok(binary_response(cached));
    }

    let payload = fetch_block_payload(&state, height).await?;
    let payload = Arc::<[u8]>::from(payload);

    state.cache.lock().await.put(height, payload.clone());
    Ok(binary_response(payload))
}

async fn get_block_meta(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    let block_hash = fetch_text(
        &state.client,
        format!("{}/block-height/{}", state.mempool_base_url, height),
    )
    .await?;
    let block: Block = fetch_json(
        &state.client,
        format!("{}/block/{}", state.mempool_base_url, block_hash),
    )
    .await?;

    Ok(axum::Json(BlockMeta {
        id: block.id,
        height: block.height,
        timestamp: block.timestamp,
        size: block.size,
        tx_count: block.tx_count,
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

async fn fetch_block_payload(state: &AppState, height: u64) -> Result<Vec<u8>, AppError> {
    let block_hash = fetch_text(
        &state.client,
        format!("{}/block-height/{}", state.mempool_base_url, height),
    )
    .await?;
    let block: Block = fetch_json(
        &state.client,
        format!("{}/block/{}", state.mempool_base_url, block_hash),
    )
    .await?;

    let mut transactions = Vec::with_capacity(block.tx_count);
    for offset in (0..block.tx_count).step_by(PAGE_SIZE) {
        let mut page: Vec<Tx> = fetch_json(
            &state.client,
            format!(
                "{}/block/{}/txs/{}",
                state.mempool_base_url, block.id, offset
            ),
        )
        .await?;
        transactions.append(&mut page);
    }

    encode_transactions(&transactions)
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

async fn fetch_json<T: for<'de> Deserialize<'de>>(
    client: &Client,
    url: String,
) -> Result<T, AppError> {
    let response = client
        .get(url)
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

fn encode_transactions(transactions: &[Tx]) -> Result<Vec<u8>, AppError> {
    let mut payload = Vec::with_capacity(transactions.len() * 33);
    for tx in transactions {
        let txid = hex::decode(&tx.txid).map_err(|_| AppError::invalid_txid(tx.txid.clone()))?;
        if txid.len() != 32 {
            return Err(AppError::invalid_txid(tx.txid.clone()));
        }
        let sum_vout: u64 = tx
            .vout
            .iter()
            .flatten()
            .filter_map(|o| o.value)
            .sum();
        let display_size = log_tx_size(sum_vout);
        payload.extend_from_slice(&txid);
        payload.push(display_size);
    }
    Ok(payload)
}

/// Matches bitfeed's logTxSize: max(1, ceil(log10(satoshis)) - 5)
fn log_tx_size(satoshis: u64) -> u8 {
    if satoshis == 0 {
        return 1;
    }
    let log = (satoshis as f64).log10().ceil() as i64;
    (log - 5).max(1) as u8
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
    fn upstream_transport(error: reqwest::Error) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("upstream request failed: {error}"),
        }
    }

    fn upstream_status(status: u16) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("upstream returned status {status}"),
        }
    }

    fn invalid_txid(txid: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("upstream returned invalid txid: {txid}"),
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

    #[test]
    fn encodes_transactions_as_txid_plus_display_size() {
        let transactions = vec![
            Tx {
                txid: "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
                    .to_string(),
                vout: Some(vec![TxOut { value: Some(1_000_000_000) }]), // 10 BTC → size 4
            },
            Tx {
                txid: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    .to_string(),
                vout: Some(vec![TxOut { value: Some(0) }]), // 0 sats → size 1
            },
        ];

        let encoded = encode_transactions(&transactions).expect("payload should encode");

        assert_eq!(encoded.len(), 66); // 2 * 33
        assert_eq!(&encoded[..32], &hex::decode(&transactions[0].txid).unwrap());
        // log_tx_size(1_000_000_000): ceil(log10(1e9))=9, 9-5=4
        assert_eq!(encoded[32], 4);
        assert_eq!(&encoded[33..65], &hex::decode(&transactions[1].txid).unwrap());
        assert_eq!(encoded[65], 1); // 0 sats → size 1
    }

    #[test]
    fn log_tx_size_matches_bitfeed() {
        assert_eq!(log_tx_size(0), 1);                   // zero → 1
        assert_eq!(log_tx_size(100_000), 1);             // 0.001 BTC → 1
        assert_eq!(log_tx_size(10_000_000), 2);          // 0.1 BTC → 2
        assert_eq!(log_tx_size(100_000_000), 3);         // 1 BTC → 3
        assert_eq!(log_tx_size(1_000_000_000), 4);       // 10 BTC → 4
        assert_eq!(log_tx_size(2_500_000_000), 5);       // 25 BTC coinbase → 5
    }
}
