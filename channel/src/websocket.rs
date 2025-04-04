use crate::channel::{listen_to_redis, Channel, ChannelControl, ChannelError, ChannelMessage};
use crate::utils::decode_jwt;
use futures::SinkExt;
use futures::StreamExt;
use itertools::Itertools;
use redis::aio::MultiplexedConnection;
use redis::AsyncCommands;
use redis::RedisResult;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_tuple::{Deserialize_tuple, Serialize_tuple};
use std::collections::HashMap;
use std::fmt;
use std::fmt::{Display, Error};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use warp::filters::ws::WebSocket;

/// reply data structures
#[derive(Clone, Debug, Serialize_tuple)]
pub struct ServerMessage {
    pub join_ref: Option<String>, // null when it's heartbeat, or initialized from server
    pub event_ref: String,
    pub topic: String, // `channel`
    pub event: String,
    pub payload: ServerPayload,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum ServerPayload {
    ServerResponse(ServerResponse),
    ServerJsonValue(serde_json::Value),
}

#[derive(Clone, Debug, Serialize)]
pub struct ServerResponse {
    pub status: String,
    pub response: Response,
}

/// 从 websocket 反序列化的
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Response {
    #[serde(rename = "join")]
    Join { id: String },

    #[serde(rename = "heartbeat")]
    Heartbeat {},

    #[serde(rename = "datetime")]
    Datetime { datetime: String, counter: u32 },

    #[serde(rename = "message")]
    Message { message: String },

    #[serde(rename = "null")]
    Empty {},
}

impl fmt::Display for ServerMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Format the response based on its variant
        // let response_str = match &self.payload.response {
        //     Response::Empty {} => "Empty".to_string(),
        //     Response::Join {} => "Join".to_string(),
        //     Response::Heartbeat {} => "Heartbeat".to_string(),
        //     Response::Datetime { datetime, counter } => {
        //         format!("<Datetime '{}' {}>", datetime, counter)
        //     }
        //     Response::Message { message } => format!("{{message: {}}}", message),
        // };
        let join_ref = self.join_ref.clone().unwrap_or("None".to_string());

        let response_str = "...";
        let payload_display = match self.payload {
            ServerPayload::ServerResponse(ref resp) => format!("<ServerResponse status={}, response={}>", resp.status, response_str),
            ServerPayload::ServerJsonValue(ref value) => format!("<ServerJsonResponse {}>", value),
        };
        write!(f, "Message join_ref={}, ref={}, topic={}, event={}, {}", join_ref, self.event_ref, self.topic, self.event, payload_display)
    }
}

// request data structures
// RequestMessage is a message from client through websocket
// it's deserialized from a JSON array
#[derive(Debug, Deserialize_tuple)]
struct RequestMessage {
    join_ref: Option<String>, // null when it's heartbeat
    event_ref: String,
    topic: String, // `channel`
    event: String,
    payload: RequestPayload,
}

impl Display for RequestMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> Result<(), Error> {
        write!(
            formatter,
            "<RequestMessage: join_ref={:?}, ref={}, topic={}, event={}, payload=...>",
            self.join_ref, self.event_ref, self.topic, self.event
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
enum RequestPayload {
    Join { token: String },
    Message { message: String },
    JsonValue(serde_json::Value), // 这样允许提交的数据只要是JSON 就可以了
}

pub struct State {
    pub ctl: Mutex<ChannelControl>,
    pub redis_client: redis::Client,
    pub id_length: u8,
    pub jwt_secret: String,
    pub jwt_expiration_secs: i64,
}

impl State {}

pub async fn axum_on_connected(ws: axum::extract::ws::WebSocket, state: Arc<State>, user_token: Option<String>) {
    info!("params: {:?}", user_token);

    let conn_id = nanoid::nanoid!(8).to_string();
    state.ctl.lock().await.conn_add_tx(conn_id.clone()).await;
    info!("AXUM / WS_TX / new connection connected: {}", conn_id);

    let (mut ws_tx, mut ws_rx) = ws.split();

    // conn rx => ws tx
    let ws_tx_state = state.clone();
    let ws_tx_conn_id = conn_id.clone();
    let mut ws_tx_task = tokio::spawn(async move {
        info!("AXUM / WS_TX / launch websocket tx task (conn rx => ws tx) ...");

        let mut conn_rx = ws_tx_state.ctl.lock().await.conn_rx(ws_tx_conn_id.clone()).await.unwrap();
        loop {
            match conn_rx.recv().await {
                Ok(channel_message) => {
                    let ChannelMessage::Reply(reply_message) = channel_message;
                    let text_result = serde_json::to_string(&reply_message);
                    if text_result.is_err() {
                        error!("AXUM / WS_TX / fail to serialize reply message: {}", text_result.err().unwrap());
                        break;
                    }
                    let text = text_result.unwrap();
                    let sending_result = ws_tx.send(axum::extract::ws::Message::Text(text.into())).await;
                    if sending_result.is_err() {
                        error!("AXUM / WS_TX / websocket tx sending failed: {}", sending_result.err().unwrap());
                        break; // what happend? exit if the connection is lost
                    }
                }
                Err(e) => {
                    error!("AXUM / WS_TX / rx error: {:?}", e);
                    break;
                }
            }
        }
    });

    let ws_rx_state = state.clone();
    let ws_rx_conn_id = conn_id.clone();
    let ws_rx_user_token = user_token.clone();
    let mut ws_rx_task = tokio::spawn(async move {
        info!("AXUM / WS_RX / websocket rx handling (ws rx =>) ...");
        let mut redis_conn = ws_rx_state.redis_client.get_multiplexed_async_connection().await.unwrap();

        // 从 websocket rx 读取所有消息，处理或者分发到各个 channel
        loop {
            let ws_msg_opt = ws_rx.next().await;
            if ws_msg_opt.is_none() {
                error!("AXUM / WS_RX / ws rx receiving failure");
                break;
            }
            let msg_result = ws_msg_opt.unwrap();
            if msg_result.is_err() {
                error!("AXUM / WS_RX / rx error: {:?}", msg_result.err());
                break;
            }
            let msg = msg_result.unwrap();
            handle_message(ws_rx_state.clone(), ws_rx_user_token.clone(), &ws_rx_conn_id, msg.to_text().unwrap(), &mut redis_conn)
                .await
                .unwrap();
        }
    });

    // Wait for either task to finish: 一个结束了总是等另外一个
    tokio::select! {
        _ = (&mut ws_tx_task) => {
            info!("AXUM / ws_tx_task exits.");

            ws_rx_task.abort();
            info!("AXUM / ws_rx_task aborts.");
        },
        _ = (&mut ws_rx_task) => {
            info!("AXUM / ws_rx_task exits.");

            ws_tx_task.abort();
            info!("AXUM / ws_tx_task aborts.");
        },
    }

    state.ctl.lock().await.conn_cleanup(conn_id.clone()).await;
    info!("AXUM / CONNECTION CLOSED");
    // phoenix/admin/system 之外，如果是 channel 的最后一个 agent，清理 channel 相关
}

/// handle websocket connection
pub async fn warp_on_connected(ws: WebSocket, state: Arc<State>) {
    let conn_id = nanoid::nanoid!(8).to_string(); // 服务端生成的，内部使用
    state.ctl.lock().await.conn_add_tx(conn_id.clone()).await;
    info!("on_connected, 新连接: {}", conn_id);

    let (mut ws_tx, mut ws_rx) = ws.split();

    // conn rx => ws tx
    // 把消息经由 websocket 发送到客户端
    let ws_state = state.clone();
    let ws_conn_id = conn_id.clone();
    let mut ws_tx_task = tokio::spawn(async move {
        debug!("launch websocket tx task (conn rx => ws tx) ...");

        let mut conn_rx = ws_state.ctl.lock().await.conn_rx(ws_conn_id.clone()).await.unwrap();
        while let Ok(channel_message) = conn_rx.recv().await {
            let ChannelMessage::Reply(reply_message) = channel_message;
            let text = serde_json::to_string(&reply_message).unwrap();
            let result = ws_tx.send(warp::ws::Message::text(text)).await;
            if result.is_err() {
                error!("websocket tx sending failed: {}", result.err().unwrap());
                break; // what happend? exit if the connection is lost
            }
        }
    });
    // let mut ws_rx_task = tokio::spawn(websocket_rx(ws_rx, state.clone(), conn_id.clone()));
    let state_clone = state.clone();
    let conn_id_clone = conn_id.clone();
    let mut ws_rx_task = tokio::spawn(async move {
        info!("websocket rx handling (ws rx =>) ...");

        let mut redis_conn = state_clone.redis_client.get_multiplexed_async_connection().await.unwrap();

        // 从 websocket rx 读取所有消息，处理或者分发到各个 channel
        while let Some(msg_result) = ws_rx.next().await {
            if msg_result.is_err() {
                error!("ws rx receiving failure: {}", msg_result.err().unwrap());
                break;
            }
            let msg = msg_result.unwrap();
            let text = msg.to_str().unwrap();
            handle_message(state_clone.clone(), None, &conn_id_clone, text, &mut redis_conn)
                .await
                .unwrap();
        }
    });

    tokio::select! {
        _ = (&mut ws_rx_task) => {
            ws_rx_task.abort();
            info!("ws {} rx task aborted.", conn_id);
        },
        _ = (&mut ws_tx_task) => {
            ws_tx_task.abort();
            info!("ws {} tx task aborted.", conn_id);
        }
    }

    // 这个是 conn 结束，不是 agent 结束
    // state.ctl.lock().await.agent_rm(conn_id.to_string()).await;
    info!("client connection closed");
}

async fn handle_message(
    state: Arc<State>, user_token: Option<String>, conn_id: &str, text: &str, redis_conn: &mut redis::aio::MultiplexedConnection,
) -> RedisResult<()> {
    let rm_result = serde_json::from_str::<RequestMessage>(text);
    if rm_result.is_err() {
        error!("WS_RX / conn: {}, error: {:?}", &conn_id, rm_result.err());
        // 清理 conn_id 的所有 agent
        // state.ctl.lock().await.agent_rm(conn_id).await;
        return Ok(());
    }
    let rm: RequestMessage = rm_result.unwrap();
    let channel_name = &rm.topic;
    let join_ref = &rm.join_ref;
    let event_ref = &rm.event_ref;
    let event = &rm.event;
    let payload = &rm.payload;

    if channel_name == "phoenix" && event == "heartbeat" {
        // it continues to publish events to the Redis
        ok_reply(conn_id, None, event_ref, "phoenix", state.clone()).await;

        // payload 是 {}, 所以这里增加一些信息
        let message = format!(r#"{{"conn_id": "{}"}}"#, conn_id); // double {{ and }} to escape
        publish_event(redis_conn, "from:phoenix:heartbeat".to_string(), message).await;
        // continue;
    }

    if event == "phx_join" {
        // 如果没有channel，创建

        // 启动一个新的 relay task (agent rx => conn tx), 需要在agent leave 的时候清除
        // connection 中每个 join 都会产生这个 task；connection 断掉的时候会自动退出
        let _relay_task = handle_join(user_token, &rm, state.clone(), conn_id).await;
        debug!("WS_RX / join processed");
        // continue;
    }

    if event == "phx_leave" {
        handle_leave(state.clone(), conn_id, join_ref.clone(), event_ref, channel_name.clone()).await;
        debug!("WS_RX / leave processed");
    }

    // all events are dispatched to reids
    // iredis --url redis://localhost:6379 psubscribe 'from*'
    let redis_topic = format!("from:{}:{}", channel_name, event.clone());
    let message = serde_json::to_string(&payload).unwrap();
    publish_event(redis_conn, redis_topic, message).await;
    Ok(())
}

// iredis --url redis://localhost:6379 psubscribe 'from*'
async fn publish_event(redis_conn: &mut redis::aio::MultiplexedConnection, redis_topic: String, message: String) {
    let publish_result: RedisResult<String> = redis_conn.publish(redis_topic.clone(), message.clone()).await;
    if let Err(e) = publish_result {
        error!("fail to publish to redis: {}", e)
    }
}

pub fn is_special_channel(ch: &str) -> bool {
    let excludes: Vec<&str> = vec!["phoenix", "admin", "system"];
    excludes.contains(&ch)
}

pub async fn add_channel(ctl: &Mutex<ChannelControl>, channel_name: String) {
    let ctl = ctl.lock().await;

    let mut channels = ctl.channels.lock().await;
    let channel_exists = channels.contains_key(&channel_name);
    if channel_exists {
        warn!("ADD_CH / channel {} already exists", channel_name);
    }

    channels
        .entry(channel_name.clone())
        .or_insert_with(|| Channel::new(channel_name.clone(), None));
    warn!("ADD_CH / {} added", channel_name);

    let channel_names = channels.keys().cloned().collect::<Vec<String>>();
    info!("ADD_CH / {} created, channels: {} {:?}", channel_name, channel_names.len(), channel_names);

    let meta = json!({
        "channel": channel_name,
        "channels": channels.keys().cloned().collect::<Vec<String>>(),
    });
    ctl.pub_meta_event("channe".into(), "add".into(), meta).await;
}

/// launch a tokio thread to listen to redis topic
/// 每个 channel 第一个 agent 连上来的时候创建这个 thread，最后一个离开时候会销毁
/// phoenix, system, admin 这3个的是直接创建的，并一致存在
pub async fn launch_channel_redis_listen_task(state: Arc<State>, ctl: &Mutex<ChannelControl>, channel_name: String, redis_client: redis::Client) {
    let ctl = ctl.lock().await;
    let mut channels = ctl.channels.lock().await;
    let channel: &mut Channel = channels.get_mut(&channel_name).unwrap();
    if channel.redis_listen_task.is_some() {
        warn!("LAUNCH_REDIS_TASK / channel {} redis_listen_task already exists", channel_name);
        return;
    }
    channel.redis_listen_task = Some(tokio::spawn(listen_to_redis(state, channel.tx.clone(), redis_client, channel_name.clone())));
    info!("LAUNCH_REDIS_TASK / channel {} redis_listen_task launched", channel_name);
}

// 添加 agent tx, join channel, spawn agent/conn relay task, ack joining
async fn handle_join(user_token: Option<String>, rm: &RequestMessage, state: Arc<State>, conn_id: &str) -> Result<JoinHandle<()>, ChannelError> {
    // 先尝试 join payload 是否包含, 然后看 user_tokne 时候有
    let token = match &rm.payload {
        RequestPayload::Join { token } => Ok(token.clone()),
        _ => user_token.ok_or_else(|| {
            error!("JOIN / invalid payload: {:?}", rm.payload);
            ChannelError::BadToken
        }),
    }?;
    let claims = match decode_jwt(&token, state.jwt_secret.clone()).await {
        Ok(claims) => claims,
        Err(e) => {
            error!("JOIN / fail to decode JWT, {}, {}", e, token);
            return Err(ChannelError::BadToken);
        }
    };

    debug!("JOIN / claims: {:?}", claims);

    let channel_name = rm.topic.clone();
    if is_special_channel(&channel_name) {
        info!("ADD_CH / channel {} is special, ignored", channel_name);
    } else {
        add_channel(&state.ctl, channel_name.clone()).await;
        launch_channel_redis_listen_task(state.clone(), &state.ctl, channel_name.clone(), state.redis_client.clone()).await;
    }

    let agent_id = format!("{}:{}:{}", conn_id, channel_name.clone(), rm.join_ref.clone().unwrap());
    let join_ref = rm.join_ref.clone();
    let event_ref = rm.event_ref.clone();

    info!("JOIN / agent joining ({} => {}) ...", agent_id, channel_name);
    state.ctl.lock().await.agent_add(agent_id.to_string(), None).await; // Agent 并不是在这里创建的
    match state
        .ctl
        .lock()
        .await
        .channel_join(&channel_name.clone(), agent_id.to_string(), claims.id.clone())
        .await
    {
        Ok(_) => {}
        Err(e) => {
            // relay task 在连接断开的时候会发生什么?
            error!("JOIN / fail to join: {}", e);
            return Err(e);
        }
    }

    // agent rx 到 conn tx 转发消息
    // 这个需要在 join 完整之前准备好，才不会丢失消息
    let relay_state = state.clone();
    let local_join_ref = rm.join_ref.clone();
    let local_conn_id = conn_id.to_string();
    let local_agent_id = agent_id.clone();
    let relay_task = tokio::spawn(async move {
        let mut agent_rx = relay_state.ctl.lock().await.agent_rx(local_agent_id.clone()).await.unwrap();
        let conn_tx = relay_state.ctl.lock().await.conn_tx(local_conn_id.to_string()).await.unwrap();

        debug!("R / agent {} => conn {}", local_agent_id.clone(), local_conn_id.clone());
        loop {
            let message_opt = agent_rx.recv().await;
            if message_opt.is_err() {
                error!("R / fail to get message from agent rx: {}", message_opt.err().unwrap());
                break;
            }
            let mut channel_message = message_opt.unwrap();
            let ChannelMessage::Reply(ref mut reply) = channel_message;
            reply.join_ref = local_join_ref.clone();
            let send_result = conn_tx.send(channel_message.clone()); // agent rx => conn tx => conn rx => ws tx
            if send_result.is_err() {
                error!("R / agent {}, conn: {}, sending failure: {:?}, exit ...", &local_agent_id, &local_conn_id, send_result.err().unwrap());
                break; // fails when there's no reciever, connection lost, stop forwarding
            }
            // debug!("R / agent {}, sent", &local_agent_id);
        }
    });

    // phx_reply, 确认 join 事件
    ok_reply(conn_id, join_ref.clone(), &event_ref, &channel_name, state.clone()).await;
    info!("JOIN / acked");

    if channel_name == "admin" {
        info!("JOIN / handling admin initialization ...");
        let ctl = state.ctl.lock().await;
        let channels = ctl.channels.lock().await;
        for (name, channel) in channels.iter() {
            // let channel = channels.get(&channel_name).unwrap();
            let meta = json!({"channel": name, "agents": *channel.agents.lock().await});
            ctl.pub_meta_event("channel".into(), "list".into(), meta).await;
        }
    }

    // presence state, to current agent
    presence_state(conn_id, join_ref.clone(), &event_ref, &channel_name, state.clone()).await;

    // presence diff, broadcast
    let mut redis_conn = state.redis_client.get_multiplexed_async_connection().await.unwrap();
    presence_diff(&mut redis_conn, channel_name.clone(), agent_id.clone(), claims.id.clone(), PresenceAction::Join).await;

    Ok(relay_task)
}

async fn handle_leave(state: Arc<State>, conn_id: &str, join_ref: Option<String>, event_ref: &str, channel_name: String) {
    let agent_id = format!("{}:{}:{}", conn_id, channel_name, join_ref.clone().unwrap());
    let external_id_opt = state.ctl.lock().await.agent_rm(agent_id.clone()).await;
    let agent_count = state
        .ctl
        .lock()
        .await
        .channel_leave(channel_name.clone(), agent_id.clone())
        .await
        .unwrap();
    if agent_count == 0 && !is_special_channel(&channel_name) {
        warn!("LEAVE / channel {} is empty, cleaning up ...", channel_name);
        state.ctl.lock().await.channel_rm(channel_name.clone()).await; // 空的 channel 会被清理
    }
    ok_reply(conn_id, join_ref, event_ref, &channel_name, state.clone()).await;

    if external_id_opt.is_none() {
        error!("LEAVE / agent {} not found", agent_id);
        return;
    }
    info!("LEAVE / send presense_diff");
    let mut redis_conn = state.redis_client.get_multiplexed_async_connection().await.unwrap();
    presence_diff(&mut redis_conn, channel_name.clone(), agent_id.clone(), external_id_opt.unwrap(), PresenceAction::Leave).await;
}

async fn ok_reply(conn_id: &str, join_ref: Option<String>, event_ref: &str, channel_name: &str, state: Arc<State>) {
    let response = match join_ref {
        None => Response::Empty {}, // heartbeat
        Some(ref join_ref) => Response::Join {
            id: format!("{}:{}:{}", conn_id, channel_name, join_ref),
        }, // join
    };
    let join_reply_message = ServerMessage {
        join_ref: join_ref.clone(),
        event_ref: event_ref.to_string(),
        topic: channel_name.to_string(),
        event: "phx_reply".to_string(),
        payload: ServerPayload::ServerResponse(ServerResponse {
            status: "ok".to_string(),
            response,
        }),
    };
    state
        .ctl
        .lock()
        .await
        .conn_send(conn_id.to_string(), ChannelMessage::Reply(join_reply_message))
        .await
        .unwrap();
    // let text = serde_json::to_string(&join_reply_message).unwrap();
    // debug!("sent to connection {}: {}", &conn_id, text);
}

async fn presence_state(conn_id: &str, join_ref: Option<String>, event_ref: &str, channel_name: &str, state: Arc<State>) {
    let hashed_agents = state
        .ctl
        .lock()
        .await
        .agents
        .lock()
        .await
        .iter()
        .filter(|(_, agent)| agent.channel == channel_name)
        .into_group_map_by(|(_, agent)| &agent.external_id)
        .into_iter()
        .map(|(external_id, group)| {
            (external_id.clone(), {
                json!({
                    "metas": group.into_iter()
                        .map(|(id, _)| json!({ "phx_ref": id }))
                        .collect::<Vec<_>>()
                })
            })
        })
        .collect::<HashMap<_, _>>();
    let reply = ServerMessage {
        join_ref: join_ref.clone(),
        event_ref: event_ref.to_string(),
        topic: channel_name.to_string(),
        event: "presence_state".to_string(),
        payload: ServerPayload::ServerJsonValue(json!(hashed_agents)),
    };
    state
        .ctl
        .lock()
        .await
        .conn_send(conn_id.to_string(), ChannelMessage::Reply(reply))
        .await
        .unwrap();
    info!("P_STATE / sent");
}

#[derive(Debug)]
pub enum PresenceAction {
    Join,
    Leave,
}

pub async fn presence_diff_many(redis_conn: &mut MultiplexedConnection, channel_name: String, action: PresenceAction, items: serde_json::Value) {
    let diff = match action {
        PresenceAction::Join => json!({"joins": items, "leaves": {}}),
        PresenceAction::Leave => json!({"joins": {}, "leaves": items}),
    };
    let redis_topic = format!("to:{}:presence_diff", channel_name);
    let message = serde_json::to_string(&diff).unwrap();
    let publish_result: RedisResult<String> = redis_conn.publish(redis_topic.clone(), message.clone()).await;
    if let Err(e) = publish_result {
        error!("P_DIFF_MANY / fail to publish to redis: {}", e)
    } else {
        info!("P_DIFF_MANY / sent, {:?}", action);
    }
}

/// broadcast presence_diff oever redis
pub async fn presence_diff(
    redis_conn: &mut MultiplexedConnection, channel_name: String, agent_id: String, external_id: String, action: PresenceAction,
) {
    let items = json!({
        external_id.clone(): {
            "metas": [
                {"phx_ref": agent_id.clone()}
            ],
        },
    });
    let diff = match action {
        PresenceAction::Join => json!({"joins": items, "leaves": {}}),
        PresenceAction::Leave => json!({"joins": {}, "leaves": items}),
    };
    let redis_topic = format!("to:{}:presence_diff", channel_name);
    let message = serde_json::to_string(&diff).unwrap();
    let publish_result: RedisResult<String> = redis_conn.publish(redis_topic.clone(), message.clone()).await;
    if let Err(e) = publish_result {
        error!("P_DIFF / fail to publish to redis: {}", e)
    } else {
        info!("P_DIFF / sent, {:?}", action);
    }
}

// 每秒发送一个时间戳
pub async fn datetime_handler(state: Arc<State>, channel_name: String) {
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    info!("launch system/datetime task...");
    let mut counter = 0;
    let event = "datetime";
    loop {
        let now = chrono::Local::now();
        let message = ServerMessage {
            join_ref: None,
            event_ref: counter.to_string(),
            topic: channel_name.to_string(),
            event: event.to_string(),
            payload: ServerPayload::ServerResponse(ServerResponse {
                status: "ok".to_string(),
                response: Response::Datetime {
                    datetime: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
                    counter,
                },
            }),
        };
        match state
            .ctl
            .lock()
            .await
            .channel_broadcast(channel_name.to_string(), ChannelMessage::Reply(message))
            .await
        {
            Ok(0) => {} // no client
            Ok(_) => {} // debug!("datetime > {}", text),
            Err(ChannelError::ChannelEmpty) => {}
            Err(e) => {
                error!("DT / fail to broadcast, channel: {}, event: {}, error: {}", channel_name, event, e);
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Query, State as AxumState, WebSocketUpgrade},
        response::IntoResponse,
        routing::get,
        Router,
    };
    use futures::{SinkExt, StreamExt};
    use serde::Deserialize;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;
    use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

    #[derive(Debug, Deserialize)]
    struct WebSocketParams {
        #[serde(rename = "userToken")]
        user_token: Option<String>,

        #[serde(rename = "vsn")]
        version: Option<String>,
    }

    async fn axum_websocket_handler(
        ws: WebSocketUpgrade, Query(params): Query<WebSocketParams>, AxumState(state): AxumState<Arc<State>>,
    ) -> impl IntoResponse {
        let user_token = params.user_token.clone();
        ws.on_upgrade(move |socket| axum_on_connected(socket, state, user_token))
    }

    async fn setup_test_server() -> (String, Arc<State>) {
        let redis_url = "redis://127.0.0.1:6379".to_string();
        let redis_client = redis::Client::open(redis_url.clone()).unwrap();
        let channel_control = ChannelControl::new(Arc::new(redis_client.clone()));
        let state = Arc::new(State {
            ctl: Mutex::new(channel_control),
            redis_client,
            jwt_secret: "secret".into(),
        });

        // Setup channels
        state.ctl.lock().await.channel_add("phoenix".into(), None).await;
        state.ctl.lock().await.channel_add("system".into(), None).await;
        state.ctl.lock().await.channel_add("streaming".into(), None).await;

        // Spawn system task
        tokio::spawn(datetime_handler(state.clone(), "system".into()));

        // Create Axum router with websocket handler
        let app = Router::new().route("/websocket", get(axum_websocket_handler)).with_state(state.clone());

        // Bind to a random available port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("ws://127.0.0.1:{}/websocket", port);

        // Start the server
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Return the websocket URL and state
        (addr, state)
    }

    async fn connect_client(
        addr: &str,
    ) -> (
        futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, tokio_tungstenite::tungstenite::Message>,
        futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) {
        let (ws_stream, _) = connect_async(addr).await.expect("Failed to connect");
        ws_stream.split()
    }

    #[tokio::test]
    async fn test_ws_websocket_connection() {
        let (addr, _) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Test initial connection with heartbeat
        let heartbeat = r#"[null,"1","phoenix","heartbeat",{}]"#;
        tx.send(Message::text(heartbeat)).await.unwrap();

        if let Some(Ok(msg)) = rx.next().await {
            let response: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(response[2], "phoenix");
            assert_eq!(response[4]["status"], "ok");
        }
    }

    #[tokio::test]
    async fn test_ws_channel_join_leave_flow() {
        let (addr, state) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Join system channel
        let join_msg = r#"["1","ref1","system","phx_join",{"token":"test"}]"#;
        tx.send(Message::text(join_msg)).await.unwrap();

        // Wait for and verify join response (may need to skip other messages)
        let mut join_confirmed = false;
        // Use a longer timeout and handle timeout error properly
        let timeout_duration = std::time::Duration::from_secs(5);
        'join_loop: for _ in 0..10 {
            // Try up to 10 messages before giving up
            match tokio::time::timeout(timeout_duration, rx.next()).await {
                Ok(Some(Ok(msg))) => {
                    let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();

                    // Print for debugging purposes during test development
                    // println!("Received message: {}", resp);

                    // Check if this is the join response we're looking for
                    if resp[1] == "ref1" && resp[2] == "system" && resp[3] == "phx_reply" {
                        assert_eq!(resp[4]["status"], "ok");
                        join_confirmed = true;
                        break 'join_loop;
                    }
                    // Otherwise it's some other message like presence_state - ignore and continue
                }
                Ok(Some(Err(_))) => panic!("WebSocket error during join"),
                Ok(None) => panic!("WebSocket closed unexpectedly during join"),
                Err(_) => panic!("Timed out waiting for join response"),
            }
        }

        assert!(join_confirmed, "Never received join confirmation after reading multiple messages");

        // Check system channel has our agent
        {
            let ctl = state.ctl.lock().await;
            let channels = ctl.channels.lock().await;
            let agents = channels.get("system").unwrap().agents.lock().await;
            assert_eq!(agents.len(), 1);
        }

        // Leave channel
        let leave_msg = r#"["1","ref2","system","phx_leave",{}]"#;
        tx.send(Message::text(leave_msg)).await.unwrap();

        // Wait for and verify leave response
        let mut leave_confirmed = false;
        'leave_loop: for _ in 0..10 {
            // Try up to 10 messages before giving up
            match tokio::time::timeout(timeout_duration, rx.next()).await {
                Ok(Some(Ok(msg))) => {
                    let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();

                    // Print for debugging purposes during test development
                    // println!("Received message: {}", resp);

                    // Check if this is the leave response we're looking for
                    if resp[1] == "ref2" && resp[2] == "system" && resp[3] == "phx_reply" {
                        assert_eq!(resp[4]["status"], "ok");
                        leave_confirmed = true;
                        break 'leave_loop;
                    }
                    // Otherwise it's some other message - ignore and continue
                }
                Ok(Some(Err(_))) => panic!("WebSocket error during leave"),
                Ok(None) => panic!("WebSocket closed unexpectedly during leave"),
                Err(_) => panic!("Timed out waiting for leave response"),
            }
        }

        assert!(leave_confirmed, "Never received leave confirmation after reading multiple messages");

        // Verify channel state
        {
            let ctl = state.ctl.lock().await;
            let channels = ctl.channels.lock().await;
            let agents = channels.get("system").unwrap().agents.lock().await;
            assert_eq!(agents.len(), 0);
        }

        // Explicitly close the connection and wait for it to finish
        drop(tx);
        drop(rx);

        // Give time for cleanup tasks to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn test_ws_connection_close_websocket() {
        let (addr, state) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Join system channel
        let join_msg = r#"["1","ref1","system","phx_join",{"token":"test"}]"#;
        tx.send(Message::text(join_msg)).await.unwrap();

        // Wait for join response
        if let Some(Ok(_)) = rx.next().await {}

        // Give time for join to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify agent joined
        let agent_count = {
            let ctl = state.ctl.lock().await;
            let channels = ctl.channels.lock().await;
            let agents = channels.get("system").unwrap().agents.lock().await;
            agents.len()
        };
        assert_eq!(agent_count, 1, "Agent should be joined");

        // Close connection by dropping handles
        drop(tx);
        drop(rx);

        // Give time for cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Verify agent was removed
        let agent_count = {
            let ctl = state.ctl.lock().await;
            let channels = ctl.channels.lock().await;
            let agents = channels.get("system").unwrap().agents.lock().await;
            agents.len()
        };
        assert_eq!(agent_count, 0, "Agent should be removed after connection close");
    }

    // Add a new test for multiple clients that doesn't depend on specific agent IDs
    #[tokio::test]
    async fn test_ws_multiple_clients_fixed() {
        let (addr, state) = setup_test_server().await;

        // Connect and join with multiple clients
        let mut clients = vec![];
        for i in 0..3 {
            let (mut tx, mut rx) = connect_client(&addr).await;

            // Join system channel
            let join_msg = format!(r#"["{}","ref{}","system","phx_join",{{"token":"test"}}]"#, i, i);
            tx.send(Message::text(join_msg)).await.unwrap();

            // Verify join response
            if let Some(Ok(_)) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.next()).await.unwrap() {}

            clients.push((tx, rx));
        }

        // Give time for all joins to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify channel has 3 agents
        let agent_count = {
            let ctl = state.ctl.lock().await;
            let channels = ctl.channels.lock().await;
            let system_channel = channels.get("system").unwrap();
            let agents = system_channel.agents.lock().await;
            agents.len()
        };

        assert_eq!(agent_count, 3, "Should have 3 agents connected");

        // Clean up
        drop(clients);
    }

    #[tokio::test]
    async fn test_ws_flow_server() {
        let (_addr, state) = setup_test_server().await;

        let ctl = state.ctl.lock().await;
        let channels = ctl.channels.lock().await;

        let channel_names: HashSet<String> = channels.keys().cloned().collect();
        assert_eq!(
            channel_names,
            ["phoenix", "system", "streaming"]
                .iter()
                .map(|&s| s.to_string())
                .collect::<HashSet<String>>()
        );

        let agents = channels.get("system").unwrap().agents.lock().await;
        assert_eq!(agents.len(), 0);
    }

    #[tokio::test]
    async fn test_ws_flow_join_leave() {
        let (addr, state) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Join system channel
        let join_msg = r#"["1","ref1","system","phx_join",{"token":"test"}]"#;
        tx.send(Message::text(join_msg)).await.unwrap();

        // Verify join response
        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[1], "ref1");
            assert_eq!(resp[2], "system");
            assert_eq!(resp[3], "phx_reply".to_string());
            assert_eq!(resp[4]["status"], "ok");
        }

        {
            let ctl = state.ctl.lock().await;
            let channels = ctl.channels.lock().await;
            let agents = channels.get("system").unwrap().agents.lock().await;
            // 没法知道具体的 agent_id
            // assert_eq!(*agents, vec!["foobar"]);
            assert_eq!(agents.len(), 1);
        }

        // Leave channel
        let leave_msg = r#"["1","ref2","system","phx_leave",{}]"#;
        tx.send(Message::text(leave_msg)).await.unwrap();

        // Verify leave response
        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[1], "ref2");
            assert_eq!(resp[2], "system");
            assert_eq!(resp[3], "phx_reply".to_string());
            assert_eq!(resp[4]["status"], "ok");
        }

        {
            let ctl = state.ctl.lock().await;
            let channels = ctl.channels.lock().await;
            let agents = channels.get("system").unwrap().agents.lock().await;
            assert_eq!(*agents, vec!["foobar"]);
            assert_eq!(agents.len(), 0);
        }
    }

    #[tokio::test]
    async fn test_ws_multiple_clients() {
        let (addr, state) = setup_test_server().await;

        // Connect multiple clients
        let mut clients = vec![];
        assert_eq!(clients.len(), 0);

        for i in 0..3 {
            let (mut tx, mut rx) = connect_client(&addr).await;

            // Join system channel
            let join_msg = format!(r#"["{}","ref{}","system","phx_join",{{"token":"test"}}]"#, i, i);
            tx.send(Message::text(join_msg)).await.unwrap();

            // Verify join
            if let Some(Ok(msg)) = rx.next().await {
                let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
                assert_eq!(resp[4]["status"], "ok");
            }
            clients.push((tx, rx));
        }

        assert_eq!(clients.len(), 3);

        let ctl = state.ctl.lock().await;
        let channels = ctl.channels.lock().await;

        let channel_names: HashSet<String> = channels.keys().cloned().collect();
        assert_eq!(
            channel_names,
            ["phoenix", "system", "streaming"]
                .iter()
                .map(|&s| s.to_string())
                .collect::<HashSet<String>>()
        );

        let agents = channels.get("system").unwrap().agents.lock().await;
        assert_eq!(*agents, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn test_ws_message_broadcast() {
        let (addr, state) = setup_test_server().await;
        let (mut tx1, mut rx1) = connect_client(&addr).await;
        let (mut tx2, mut rx2) = connect_client(&addr).await;

        // Both clients join system channel
        for (tx, i) in [(&mut tx1, 1), (&mut tx2, 2)] {
            let join_msg = format!(r#"["{}","ref{}","system","phx_join",{{"token":"test"}}]"#, i, i);
            tx.send(Message::text(join_msg)).await.unwrap();

            // Wait for join response
            if let Some(Ok(_)) = if i == 1 { rx1.next().await } else { rx2.next().await } {}
        }

        // Broadcast message to system channel
        let message = ServerMessage {
            join_ref: None,
            event_ref: "broadcast".to_string(),
            topic: "system".to_string(),
            event: "test".to_string(),
            payload: ServerPayload::ServerResponse(ServerResponse {
                status: "ok".to_string(),
                response: Response::Message {
                    message: "test broadcast".to_string(),
                },
            }),
        };

        state
            .ctl
            .lock()
            .await
            .channel_broadcast("system".to_string(), ChannelMessage::Reply(message))
            .await
            .unwrap();

        // Both clients should receive the message
        for rx in [&mut rx1, &mut rx2] {
            if let Some(Ok(msg)) = rx.next().await {
                let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
                assert_eq!(resp[1], "broadcast");
                assert_eq!(resp[4]["response"]["message"], "test broadcast");
            }
        }
    }

    #[tokio::test]
    async fn test_ws_invalid_messages() {
        let (addr, _) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Send invalid JSON
        tx.send(Message::text("invalid json")).await.unwrap();

        // Send invalid message format
        tx.send(Message::text(r#"["invalid","format"]"#)).await.unwrap();

        // Send to non-existent channel
        let invalid_channel = r#"["1","ref1","nonexistent","phx_join",{"token":"test"}]"#;
        tx.send(Message::text(invalid_channel)).await.unwrap();

        // Connection should still be alive
        let heartbeat = r#"[null,"1","phoenix","heartbeat",{}]"#;
        tx.send(Message::text(heartbeat)).await.unwrap();

        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[2], "phoenix");
            assert_eq!(resp[4]["status"], "ok");
        }
    }

    #[tokio::test]
    async fn test_ws_system_channel() {
        let (addr, _) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Join system channel
        let join_msg = r#"["1","ref1","system","phx_join",{"token":"test"}]"#;
        tx.send(Message::text(join_msg)).await.unwrap();

        // Should receive initial join response
        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[2], "system");
            assert_eq!(resp[4]["status"], "ok");
        }

        // Should receive datetime updates
        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[2], "system");
            assert!(resp[4]["response"]["datetime"].is_string());
        }
    }

    // #[test]
    // fn test_response_invalid_json() {
    //     // Missing type field
    //     let json = r#"{"message": "hello"}"#;
    //     assert!(serde_json::from_str::<RedisResponse>(json).is_err());
    //
    //     // Invalid type value
    //     let json = r#"{"type": "invalid"}"#;
    //     assert!(serde_json::from_str::<RedisResponse>(json).is_err());
    //
    //     // Missing required fields
    //     let json = r#"{"type": "datetime", "datetime": "2024-01-01"}"#;
    //     assert!(serde_json::from_str::<RedisResponse>(json).is_err());
    // }

    #[test]
    fn test_ws_request_json_heartbeat() {
        // Test full message with join payload
        let msg: RequestMessage = serde_json::from_str(r#"["1", "ref1", "room123", "heartbeat", {}]"#).unwrap();

        assert_eq!(msg.join_ref, Some("1".to_string()));
        assert_eq!(msg.event_ref, "ref1");
        assert_eq!(msg.topic, "room123");
        assert_eq!(msg.event, "heartbeat");
        assert_eq!(msg.payload, serde_json::from_value(json!({})).unwrap());
    }

    #[test]
    fn test_ws_request_json_join() {
        // Test full message with join payload
        let msg: RequestMessage = serde_json::from_str(r#"["1", "ref1", "room123", "phx_join", {"token": "secret_token"}]"#).unwrap();

        assert_eq!(msg.join_ref, Some("1".to_string()));
        assert_eq!(msg.event_ref, "ref1");
        assert_eq!(msg.topic, "room123");
        assert_eq!(msg.event, "phx_join");
        assert_eq!(
            msg.payload,
            RequestPayload::Join {
                token: "secret_token".to_string()
            }
        );
    }

    #[test]
    fn test_ws_request_json_message() {
        let json = r#"["1", "ref4", "room123", "message", {"message": "Hello, World!"}]"#;

        let msg: RequestMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.event, "message");
        assert_eq!(
            msg.payload,
            RequestPayload::Message {
                message: "Hello, World!".to_string()
            }
        );

        // Test just the payload
        // let payload: RequestPayload = serde_json::from_value(json!({"message": "test message"})).unwrap();
        // assert_eq!(payload, RequestPayload::Message("test message".to_string()));
    }

    #[test]
    fn test_ws_request_json_message_payload() {
        let payload: RequestPayload = serde_json::from_value(json!({})).unwrap();
        assert_eq!(payload, RequestPayload::JsonValue(json!({})));

        let payload: RequestPayload = serde_json::from_value(json!({"token": "another_token"})).unwrap();
        assert_eq!(
            payload,
            RequestPayload::Join {
                token: "another_token".to_string()
            }
        );

        let payload: RequestPayload = serde_json::from_value(json!({ "message": "test message" })).unwrap();
        assert_eq!(
            payload,
            RequestPayload::Message {
                message: "test message".to_string()
            }
        );
    }

    #[test]
    fn test_ws_request_json_invalid() {
        // Invalid array length
        assert!(serde_json::from_str::<RequestMessage>(r#"["1", "ref1", "room123"]"#).is_err());
        assert!(serde_json::from_str::<RequestMessage>(r#"["1", "ref1", "room123", "phx_join"]"#).is_err());

        // Invalid join payload (wrong format)
        assert!(serde_json::from_str::<RequestMessage>(r#"["1", "ref1", "room123", "phx_join", null]"#).is_ok());
        assert!(serde_json::from_str::<RequestMessage>(r#"["1", "ref1", "room123", "phx_join", 23]"#).is_ok());
        assert!(serde_json::from_str::<RequestMessage>(r#"["1", "ref1", "room123", "phx_join", 12.4]"#).is_ok());
        assert!(serde_json::from_str::<RequestMessage>(r#"["1", "ref1", "room123", "phx_join", "nulldirect_token"]"#).is_ok());
        assert!(serde_json::from_str::<RequestMessage>(r#"["1", "ref1", "room123", "phx_join", [1, null, "foobar"]]"#).is_ok());

        // Invalid type for number elements
        assert!(serde_json::from_str::<RequestMessage>(r#"[123, "ref1", "room123", "phx_join", {"token": "secret"}]"#).is_err());
    }
}
