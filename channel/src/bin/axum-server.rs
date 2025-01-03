use axum::{
    extract::{State as AxumState, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use channel::{
    channel::ChannelControl,
    utils::random_string,
    websocket::{add_channel, axum_on_connected, datetime_handler, State},
};
use clap::Parser;
use redis::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use tracing::{error, info};
use tracing_subscriber::{fmt::format::FmtSpan, EnvFilter};

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
        .nest_service("/", ServeDir::new("channel/src/bin")) // 需要把 html 直接包含到 binary 中，方便发布
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await.unwrap();

    info!("serving at {}:{} ...", host, port);
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
