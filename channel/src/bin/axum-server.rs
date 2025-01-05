use axum::{
    extract::{Json, State as AxumState, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use channel::{
    channel::ChannelControl,
    utils::random_string,
    websocket::{add_channel, axum_on_connected, datetime_handler, State},
};
use clap::Parser;
use jsonwebtoken::{encode, EncodingKey, Header};
use redis::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use tracing::{error, info};
use tracing_subscriber::{fmt::format::FmtSpan, EnvFilter};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
struct TokenRequest {
    channel: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    id: String,
    channel: String,
    exp: usize,
}

#[derive(Debug)]
enum TokenError {
    ChannelNotFound,
    GenerationFailed,
}

impl IntoResponse for TokenError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            TokenError::ChannelNotFound => (StatusCode::NOT_FOUND, "Channel not found"),
            TokenError::GenerationFailed => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to generate token"),
        };

        (status, message).into_response()
    }
}

async fn generate_token(AxumState(state): AxumState<Arc<State>>, Json(req): Json<TokenRequest>) -> Result<impl IntoResponse, TokenError> {
    // Check if channel exists
    let ctl = state.ctl.lock().await;
    let channels = ctl.channels.lock().await;
    if !channels.contains_key(&req.channel) {
        return Err(TokenError::ChannelNotFound);
    }

    let id = Uuid::new_v4().to_string();
    let expiration = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::hours(24))
        .expect("valid timestamp")
        .timestamp() as usize;

    let claims = Claims {
        id,
        channel: req.channel,
        exp: expiration,
    };

    let key = EncodingKey::from_secret(state.jwt_secret.as_bytes());

    match encode(&Header::default(), &claims, &key) {
        Ok(token) => Ok(Json(serde_json::json!({
            "token": token
        }))),
        Err(_) => Err(TokenError::GenerationFailed),
    }
}

async fn websocket_handler(ws: WebSocketUpgrade, AxumState(state): AxumState<Arc<State>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| axum_on_connected(socket, state))
}

// use clap to parse command line arguments
#[derive(Debug, Deserialize, Parser)]
#[command(name = "wd", about = "channel server")]
struct Options {
    #[arg(long, default_value = "127.0.0.1")]
    host: Option<String>,

    #[arg(long, default_value = "5000")]
    port: Option<u16>,

    #[arg(long, default_value = None)]
    redis_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok(); // load .env if possible

    // 设置 tracing 使用 EnvFilter
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_span_events(FmtSpan::CLOSE)
        .init();

    let options = Options::parse(); // exit on error
    if options.redis_url.is_none() {
        error!("redis_url is missing");
        return Ok(());
    }

    let redis_url = options.redis_url.unwrap();
    let redis_client = Client::open(redis_url.clone())?;
    let channel_control = ChannelControl::new(Arc::new(redis_client.clone()));

    let state = Arc::new(State {
        ctl: Mutex::new(channel_control),
        redis_client,
        jwt_secret: random_string(8), // 从命令行、环境变量中获取，或者生成一个随机的
    });

    // phoenix & admin are special
    add_channel(&state.ctl, state.redis_client.clone(), "phoenix".into()).await;
    add_channel(&state.ctl, state.redis_client.clone(), "admin".into()).await;

    // predefined channel
    add_channel(&state.ctl, state.redis_client.clone(), "system".into()).await;
    tokio::spawn(datetime_handler(state.clone(), "system".into()));

    let host = options.host.unwrap();
    let port = options.port.unwrap();

    let app = Router::new()
        .route("/websocket", get(websocket_handler))
        .route("/token", post(generate_token))
        .nest_service("/", ServeDir::new("channel/src/bin")) // 需要把 html 直接包含到 binary 中，方便发布
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await.unwrap();

    info!("serving at {}:{} ...", host, port);
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
