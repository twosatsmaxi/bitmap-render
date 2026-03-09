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
    ord_base_url: Option<String>,
    cache: Arc<Mutex<LruCache<u64, (String, Arc<[u8]>)>>>,
    tx_cache: Arc<Mutex<LruCache<u64, Arc<Vec<TxSummary>>>>>,
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
    vout: Option<Vec<TxOut>>,
}

// ── Mempool full tx (for /txs pages)
#[derive(Debug, Deserialize)]
struct MempoolTx {
    txid: String,
    weight: Option<u64>,
    size: Option<u64>,
    fee: Option<u64>,
    feerate: Option<f64>,
    vout: Option<Vec<TxOut>>,
}

// ── Slim tx summary returned by /api/block/:height/txs
#[derive(Debug, serde::Serialize, Clone)]
struct TxSummary {
    txid: Option<String>,
    vsize: u64,
    fee: Option<u64>,
    feerate: Option<f64>,
    value: u64,
}

// ── Ord API types
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
    hash: String,
    height: u64,
    transactions: Vec<OrdTx>,
}

fn varint_size(n: usize) -> u64 {
    if n < 0xfd { 1 } else if n <= 0xffff { 3 } else { 5 }
}

fn ord_tx_vsize(tx: &OrdTx) -> u64 {
    let has_witness = tx.input.iter().any(|i| !i.witness.is_empty());
    let mut base: u64 = 8; // version(4) + locktime(4)
    base += varint_size(tx.input.len());
    for inp in &tx.input {
        base += 36; // prev_output: 32 txid + 4 index
        let ss_len = inp.script_sig.len() / 2;
        base += varint_size(ss_len) + ss_len as u64 + 4; // script_sig + sequence
    }
    base += varint_size(tx.output.len());
    for out in &tx.output {
        let sp_len = out.script_pubkey.len() / 2;
        base += 8 + varint_size(sp_len) + sp_len as u64; // value + script_pubkey
    }
    if !has_witness {
        return base;
    }
    let mut witness: u64 = 2; // segwit marker + flag
    for inp in &tx.input {
        witness += varint_size(inp.witness.len());
        for item in &inp.witness {
            let l = item.len() / 2;
            witness += varint_size(l) + l as u64;
        }
    }
    base + (witness + 3) / 4
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

    let ord_base_url = std::env::var("ORD_BASE_URL").ok().map(|u| u.trim_end_matches('/').to_string());

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
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .tcp_keepalive(std::time::Duration::from_secs(15))
        .build()
        .expect("failed to build reqwest client");

    if let Some(ref url) = ord_base_url {
        info!("using ord backend: {url}");
    } else {
        info!("using mempool backend: {mempool_base_url}");
    }

    let state = AppState {
        client,
        mempool_base_url,
        ord_base_url,
        cache: Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(cache_capacity).expect("cache capacity must be non-zero"),
        ))),
        tx_cache: Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(cache_capacity).expect("cache capacity must be non-zero"),
        ))),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET])
        .expose_headers(["x-block-hash".parse::<header::HeaderName>().unwrap()]);
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/block/{height}", get(get_block))
        .route("/api/block/{height}/meta", get(get_block_meta))
        .route("/api/block/{height}/txs", get(get_block_txs))
        .route("/api/blockheight/{hash}", get(get_blockheight_by_hash))
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

async fn get_block_txs(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(cached) = state.tx_cache.lock().await.get(&height).cloned() {
        return Ok(axum::Json((*cached).clone()));
    }
    let txs = if state.ord_base_url.is_some() {
        fetch_ord_tx_summaries(&state, height).await?
    } else {
        fetch_mempool_tx_summaries(&state, height).await?
    };
    let txs = Arc::new(txs);
    state.tx_cache.lock().await.put(height, txs.clone());
    Ok(axum::Json((*txs).clone()))
}

async fn fetch_mempool_tx_summaries(state: &AppState, height: u64) -> Result<Vec<TxSummary>, AppError> {
    let block_hash = fetch_text(
        &state.client,
        format!("{}/block-height/{}", state.mempool_base_url, height),
    ).await?;
    let block: Block = fetch_json(
        &state.client,
        format!("{}/block/{}", state.mempool_base_url, block_hash),
    ).await?;

    let mut all: Vec<MempoolTx> = Vec::with_capacity(block.tx_count);
    for offset in (0..block.tx_count).step_by(PAGE_SIZE) {
        let mut page: Vec<MempoolTx> = fetch_json(
            &state.client,
            format!("{}/block/{}/txs/{}", state.mempool_base_url, block.id, offset),
        ).await?;
        all.append(&mut page);
    }

    Ok(all.into_iter().map(|tx| {
        let vsize = tx.weight.map(|w| (w + 3) / 4).or(tx.size).unwrap_or(0);
        let value = tx.vout.iter().flatten().filter_map(|o| o.value).sum();
        TxSummary { txid: Some(tx.txid), vsize, fee: tx.fee, feerate: tx.feerate, value }
    }).collect())
}

async fn fetch_ord_tx_summaries(state: &AppState, height: u64) -> Result<Vec<TxSummary>, AppError> {
    let ord_base = state.ord_base_url.as_ref().unwrap();
    let block: OrdBlock = state.client
        .get(format!("{}/block/{}", ord_base, height))
        .header("Accept", "application/json")
        .send().await.map_err(AppError::upstream_transport)?
        .json::<OrdBlock>().await.map_err(AppError::upstream_transport)?;

    Ok(block.transactions.iter().map(|tx| {
        let vsize = ord_tx_vsize(tx);
        let value: u64 = tx.output.iter().filter_map(|o| o.value).sum();
        TxSummary { txid: None, vsize, fee: None, feerate: None, value }
    }).collect())
}

async fn get_blockheight_by_hash(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    #[derive(serde::Serialize)]
    struct HeightResp { height: u64 }

    if let Some(ref ord_base) = state.ord_base_url {
        #[derive(Deserialize)]
        struct OrdBlockHeight { height: u64 }
        let b: OrdBlockHeight = state.client
            .get(format!("{}/block/{}", ord_base, hash))
            .header("Accept", "application/json")
            .send().await.map_err(AppError::upstream_transport)?
            .json().await.map_err(AppError::upstream_transport)?;
        return Ok(axum::Json(HeightResp { height: b.height }));
    }

    let block: Block = fetch_json(
        &state.client,
        format!("{}/block/{}", state.mempool_base_url, hash),
    ).await?;
    Ok(axum::Json(HeightResp { height: block.height }))
}

async fn get_block(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<Response<Body>, AppError> {
    if let Some((hash, payload)) = state.cache.lock().await.get(&height).cloned() {
        return Ok(binary_response(&hash, payload));
    }

    let (hash, payload) = fetch_block_payload(&state, height).await?;
    let payload = Arc::<[u8]>::from(payload);

    state.cache.lock().await.put(height, (hash.clone(), payload.clone()));
    Ok(binary_response(&hash, payload))
}

async fn get_block_meta(
    State(state): State<AppState>,
    Path(height): Path<u64>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(ref ord_base) = state.ord_base_url {
        let block: OrdBlock = state.client
            .get(format!("{}/block/{}", ord_base, height))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(AppError::upstream_transport)?
            .json::<OrdBlock>()
            .await
            .map_err(AppError::upstream_transport)?;
        return Ok(axum::Json(BlockMeta {
            id: block.hash,
            height: block.height,
            timestamp: 0,
            size: 0,
            tx_count: block.transactions.len(),
        }));
    }

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

async fn fetch_block_payload_from_ord(state: &AppState, height: u64) -> Result<(String, Vec<u8>), AppError> {
    let ord_base = state.ord_base_url.as_ref().unwrap();
    let block: OrdBlock = state.client
        .get(format!("{}/block/{}", ord_base, height))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(AppError::upstream_transport)?
        .json::<OrdBlock>()
        .await
        .map_err(AppError::upstream_transport)?;

    let payload: Vec<u8> = block.transactions.iter().map(|tx| {
        let sum: u64 = tx.output.iter().filter_map(|o| o.value).sum();
        log_tx_size(sum)
    }).collect();

    Ok((block.hash, payload))
}

async fn fetch_block_payload(state: &AppState, height: u64) -> Result<(String, Vec<u8>), AppError> {
    if state.ord_base_url.is_some() {
        return fetch_block_payload_from_ord(state, height).await;
    }

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

    let payload = encode_transactions(&transactions);
    Ok((block.id, payload))
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

fn encode_transactions(transactions: &[Tx]) -> Vec<u8> {
    transactions
        .iter()
        .map(|tx| {
            let sum_vout: u64 = tx.vout.iter().flatten().filter_map(|o| o.value).sum();
            log_tx_size(sum_vout)
        })
        .collect()
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
        error!(
            is_connect=%error.is_connect(),
            is_timeout=%error.is_timeout(),
            is_request=%error.is_request(),
            is_decode=%error.is_decode(),
            url=?error.url(),
            "upstream error: {error:#}"
        );
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("upstream request failed: {error:#}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_transactions_as_size_only() {
        let transactions = vec![
            Tx { vout: Some(vec![TxOut { value: Some(1_000_000_000) }]) }, // 10 BTC → size 4
            Tx { vout: Some(vec![TxOut { value: Some(0) }]) },             // 0 sats → size 1
        ];

        let encoded = encode_transactions(&transactions);

        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded[0], 4); // log_tx_size(1_000_000_000): ceil(log10(1e9))=9, 9-5=4
        assert_eq!(encoded[1], 1); // 0 sats → size 1
    }

    fn make_ord_tx(inputs: &[(&str, &[&str])], outputs: &[(&str, u64)]) -> OrdTx {
        OrdTx {
            input: inputs.iter().map(|(ss, witness)| OrdInput {
                script_sig: ss.to_string(),
                witness: witness.iter().map(|w| w.to_string()).collect(),
            }).collect(),
            output: outputs.iter().map(|(sp, value)| OrdOutput {
                script_pubkey: sp.to_string(),
                value: Some(*value),
            }).collect(),
        }
    }

    #[test]
    fn ord_tx_vsize_legacy() {
        // Non-segwit tx: 1 input (empty script_sig, no witness), 1 output (22-byte p2wpkh script)
        // base = 8 + 1(input varint) + 36 + 1(ss varint) + 0 + 4 + 1(output varint) + 8 + 1(sp varint) + 22 = 82
        let tx = make_ord_tx(&[("", &[])], &[("0014fd92b03dd4f1ab7031905b79459f7abc5a5c50cb", 1000)]);
        assert_eq!(ord_tx_vsize(&tx), 82);
    }

    #[test]
    fn ord_tx_vsize_segwit() {
        // Segwit tx matching a typical P2WPKH spend:
        // input: prev(36) + ss_len(1) + sequence(4) = 41 base
        // output: value(8) + sp_len(1) + script(22) = 31
        // base = 8 + 1 + 41 + 1 + 31 = 82
        // witness = 2(marker+flag) + 1(item count) + 1(len) + 72(sig bytes) + 1(len) + 33(pubkey) = 110
        // vsize = 82 + ceil(110/4) = 82 + 28 = 110
        let sig = "3045022100".to_string() + &"aa".repeat(35);  // 72 hex bytes = 144 hex chars
        let pubkey = "02".to_string() + &"bb".repeat(32);        // 33 hex bytes = 66 hex chars
        let tx = make_ord_tx(
            &[("", &[&sig, &pubkey])],
            &[("0014fd92b03dd4f1ab7031905b79459f7abc5a5c50cb", 1000)],
        );
        // sig = 40 bytes, pubkey = 33 bytes
        // witness = 2(marker+flag) + 1(item count) + 1(sig len)+40 + 1(pubkey len)+33 = 78
        // vsize = 82 + ceil(78/4) = 82 + 20 = 102
        assert_eq!(ord_tx_vsize(&tx), 82 + (78 + 3) / 4);
    }

    #[test]
    fn tx_summary_from_ord_block() {
        let tx = make_ord_tx(
            &[("", &[])],
            &[
                ("0014fd92b03dd4f1ab7031905b79459f7abc5a5c50cb", 5_000_000),
                ("001424f39209574e117d51ebb72ae505bc6d56c26d1c", 1_000_000),
            ],
        );
        let vsize = ord_tx_vsize(&tx);
        let value: u64 = tx.output.iter().filter_map(|o| o.value).sum();
        assert_eq!(value, 6_000_000);
        assert!(vsize > 0);
        assert!(vsize < 1000); // sanity bound
    }

    #[test]
    fn mempool_tx_summary_vsize_from_weight() {
        // weight 441 → vsize = ceil(441/4) = 111
        let tx = MempoolTx {
            txid: "abc".into(),
            weight: Some(441),
            size: Some(200),
            fee: Some(1500),
            feerate: Some(13.51),
            vout: Some(vec![TxOut { value: Some(500_000) }, TxOut { value: Some(200_000) }]),
        };
        let vsize = tx.weight.map(|w| (w + 3) / 4).or(tx.size).unwrap_or(0);
        let value: u64 = tx.vout.iter().flatten().filter_map(|o| o.value).sum();
        assert_eq!(vsize, 111);
        assert_eq!(value, 700_000);
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
