use std::{
    collections::{hash_map::Entry, HashMap},
    error::Error,
    fmt::{self, Display},
    sync::atomic::{AtomicU32, Ordering},
};

use serde::Serialize;
use tokio::{
    sync::{broadcast, Mutex},
    task::JoinHandle,
};
use tracing::{debug, info};

use crate::websocket::ReplyMessage;

#[derive(Clone, Debug, Serialize)]
pub enum ChannelMessage {
    Reply(ReplyMessage),
    ReloadFilter { agent_id: String, code: String },
}

/// agent channel, can broadcast to every agent in the channel
pub struct Channel {
    /// channel name
    pub name: String,
    /// broadcast in channels
    sender: broadcast::Sender<ChannelMessage>,
    /// channel agents
    agents: Mutex<Vec<String>>,
    /// channel agent count
    count: AtomicU32,
}

/// manages all channels
pub struct ChannelControl {
    pub channel_map: Mutex<HashMap<String, Channel>>, // channel name -> Channel

    /// agent_id -> Vec<agentTask>
    /// task forwarding channel messages to agent websocket tx
    /// created when agent joins a channel
    agent_task_map: Mutex<HashMap<String, Vec<ChannelAgent>>>,

    conn_sender_map: Mutex<HashMap<String, broadcast::Sender<ChannelMessage>>>, // conn_id -> Sender
    agent_sender_map: Mutex<HashMap<String, broadcast::Sender<ChannelMessage>>>, // agent_id -> Sender
}

#[derive(Debug)]
pub enum ChannelError {
    /// channel does not exist
    ChannelNotFound,
    /// can not send message to channel
    MessageSendError,
    /// you have not called init_agent
    AgentNotInitiated,
}

impl Error for ChannelError {}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ChannelError::ChannelNotFound => {
                write!(f, "<ChannelNotFound: channel not found>")
            }
            ChannelError::AgentNotInitiated => {
                write!(f, "<AgentNotInitiated: agent not initiated>")
            }
            ChannelError::MessageSendError => {
                write!(
                    f,
                    "<MessageSendError: failed to send a message to the channel>"
                )
            }
        }
    }
}

struct ChannelAgent {
    channel_name: String,
    join_task: JoinHandle<()>,
}

impl Display for ChannelAgent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "<ChannelAgent: channel={}, task={:?}>",
            self.channel_name, self.join_task
        )
    }
}

impl Channel {
    // capacity is the maximum number of messages that can be stored in the channel
    pub fn new(name: String, capacity: Option<usize>) -> Channel {
        let (tx, _rx) = broadcast::channel(capacity.unwrap_or(100));
        Channel {
            name,
            sender: tx,
            agents: Mutex::new(vec![]),
            count: AtomicU32::new(0),
        }
    }

    /// agent joins the channel, returns a sender to the channel
    /// if agent does not exist, a new agent is added
    pub async fn join(&self, agent_id: String) -> broadcast::Sender<ChannelMessage> {
        let mut agents = self.agents.lock().await;
        if !agents.contains(&agent_id) {
            agents.push(agent_id);
            self.count.fetch_add(1, Ordering::SeqCst);
        }
        self.sender.clone()
    }

    pub async fn leave(&self, agent: String) {
        let mut agents = self.agents.lock().await;
        if let Some(pos) = agents.iter().position(|x| *x == agent) {
            agents.swap_remove(pos);
            self.count.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// broadcast messages to the channel
    /// it returns the number of agents who received the message
    pub fn send(
        &self,
        data: ChannelMessage,
    ) -> Result<usize, broadcast::error::SendError<ChannelMessage>> {
        self.sender.send(data)
    }

    pub fn empty(&self) -> bool {
        self.count.load(Ordering::SeqCst) == 0
    }

    pub async fn agents(&self) -> tokio::sync::MutexGuard<Vec<String>> {
        self.agents.lock().await
    }
}

impl ChannelControl {
    pub fn new() -> Self {
        ChannelControl {
            channel_map: Mutex::new(HashMap::new()),
            agent_task_map: Mutex::new(HashMap::new()),
            agent_sender_map: Mutex::new(HashMap::new()),
            conn_sender_map: Mutex::new(HashMap::new()),
        }
    }

    pub async fn add_connection(&self, name: String) {
        let mut conn_sender_map = self.conn_sender_map.lock().await;
        match conn_sender_map.entry(name.clone()) {
            Entry::Vacant(entry) => {
                let (tx, _rx) = broadcast::channel(100);
                entry.insert(tx);
                debug!("conn {} added", name.clone());
            }
            Entry::Occupied(_) => {}
        }
    }

    pub async fn remove_connection(&self, name: String) {}
    pub async fn get_conn_subscription(
        &self,
        conn_id: String,
    ) -> Result<broadcast::Receiver<ChannelMessage>, ChannelError> {
        info!("get conn {} subscription", conn_id);
        let conn_sender_map = self.conn_sender_map.lock().await;
        let rx = conn_sender_map.get(&conn_id).unwrap().subscribe();
        Ok(rx)
    }

    pub async fn get_conn_sender(
        &self,
        conn_id: String,
    ) -> Result<broadcast::Sender<ChannelMessage>, ChannelError> {
        info!("get conn {} sender", conn_id);
        let conn_sender_map = self.conn_sender_map.lock().await;
        Ok(conn_sender_map.get(&conn_id).unwrap().clone())
    }

    pub async fn new_channel(&self, name: String, capacity: Option<usize>) {
        let mut channels = self.channel_map.lock().await;
        channels.insert(name.clone(), Channel::new(name, capacity));
    }

    pub async fn remove_channel(&self, channel_name: String) {
        match self.channel_map.lock().await.entry(channel_name.clone()) {
            Entry::Vacant(_) => {}
            Entry::Occupied(el) => {
                for agent in el.get().agents().await.iter() {
                    if let Entry::Occupied(mut agent_tasks) =
                        self.agent_task_map.lock().await.entry(agent.into())
                    {
                        let vecotr = agent_tasks.get_mut();
                        vecotr.retain(|task| {
                            if task.channel_name == channel_name {
                                task.join_task.abort();
                            }
                            task.channel_name != channel_name
                        });
                    }
                }

                el.remove();
            }
        }
    }

    pub async fn send_to_connction(
        &self,
        conn_id: String,
        message: ChannelMessage,
    ) -> Result<usize, ChannelError> {
        self.conn_sender_map
            .lock()
            .await
            .get(&conn_id)
            .ok_or(ChannelError::ChannelNotFound)?
            .send(message)
            .map_err(|_| ChannelError::MessageSendError)
    }

    /// broadcast message to the channel
    /// it returns the number of agents who received the message
    pub async fn broadcast(
        &self,
        channel_name: String,
        message: ChannelMessage,
    ) -> Result<usize, ChannelError> {
        self.channel_map
            .lock()
            .await
            .get(&channel_name)
            .ok_or(ChannelError::ChannelNotFound)?
            .send(message)
            .map_err(|_| ChannelError::MessageSendError)
    }

    // pub async fn get_agent_sender(
    //     &self,
    //     agent_id: String,
    // ) -> Result<broadcast::Sender<ChannelMessage>, ChannelError> {
    //     info!("get agent {} sender", agent_id);
    //     let agent_sender_map = self.agent_sender_map.lock().await;
    //     Ok(agent_sender_map.get(&agent_id).unwrap().clone())
    // }

    pub async fn get_agent_subscription(
        &self,
        agent_id: String,
    ) -> Result<broadcast::Receiver<ChannelMessage>, ChannelError> {
        info!("get agent {} reciever", agent_id);
        let agent_sender_map = self.agent_sender_map.lock().await;
        let receiver = agent_sender_map
            .get(&agent_id)
            .ok_or(ChannelError::AgentNotInitiated)?
            .subscribe();
        Ok(receiver)
    }

    /// Add channel agent to the channel ctl
    /// `capacity` is the maximum number of messages that can be stored in the channel, default is 100
    /// This will create a broadcast channel: ChannelAgent will write to and websocket_tx_task will
    /// subscribe to and read from
    pub async fn add_agent(&self, agent_id: String, capacity: Option<usize>) {
        let mut agent_sender_map = self.agent_sender_map.lock().await;
        match agent_sender_map.entry(agent_id.clone()) {
            Entry::Vacant(entry) => {
                let (tx, _rx) = broadcast::channel(capacity.unwrap_or(100));
                entry.insert(tx);
                info!("agent {} added", agent_id.clone());
            }
            Entry::Occupied(_) => {
                info!("agent {} already exists", agent_id.clone());
            }
        }
    }

    /// remove the agent after leaving all channels
    pub async fn remove_agent(&self, agent_id: String) {
        let channels = self.channel_map.lock().await;
        let mut agent_tasks = self.agent_task_map.lock().await;
        let mut agent_sender_map = self.agent_sender_map.lock().await;

        match agent_tasks.entry(agent_id.clone()) {
            Entry::Occupied(agent_tasks) => {
                let tasks = agent_tasks.get();
                for task in tasks {
                    let channel = channels.get(&task.channel_name);
                    if let Some(channel) = channel {
                        channel.leave(agent_id.clone()).await;
                        debug!("agent {} removed from channel {}", agent_id, task)
                    }
                    task.join_task.abort();
                }
                agent_tasks.remove();
                debug!("agent {} tasks removed", agent_id);
            }
            Entry::Vacant(_) => {}
        }

        match agent_sender_map.entry(agent_id.clone()) {
            Entry::Occupied(entry) => {
                entry.remove();
                debug!("agent {} receiver removed", agent_id);
            }
            Entry::Vacant(_) => {}
        }
    }

    /// join agent to channel
    /// This will subscribe to the channel, create a task to forward messages to the agent websocket
    pub async fn join_channel(
        &self,
        channel_name: &str,
        agent_id: String,
    ) -> Result<broadcast::Sender<ChannelMessage>, ChannelError> {
        let channel_map = self.channel_map.lock().await;
        let mut agent_task_map = self.agent_task_map.lock().await;
        let agent_sender_map = self.agent_sender_map.lock().await;

        let channel_sender = channel_map
            .get(channel_name)
            .ok_or(ChannelError::ChannelNotFound)?
            .join(agent_id.clone())
            .await;
        let mut channel_sub = channel_sender.subscribe();
        let agent_tx = agent_sender_map
            .get(&agent_id)
            .ok_or(ChannelError::AgentNotInitiated)?
            .clone();

        /// a task for this join
        /// channel subscription to agent sender
        let join_task = tokio::spawn(channel_sub_to_agent(channel_sub, agent_tx));

        match agent_task_map.entry(agent_id.clone()) {
            Entry::Occupied(mut entry) => {
                let agent_tasks = entry.get_mut();
                if !agent_tasks.iter().any(|x| x.channel_name == channel_name) {
                    agent_tasks.push(ChannelAgent {
                        channel_name: channel_name.to_string().clone(),
                        join_task,
                    });
                }
            }
            Entry::Vacant(v) => {
                v.insert(vec![ChannelAgent {
                    channel_name: channel_name.to_string().clone(),
                    join_task,
                }]);
            }
        };
        Ok(channel_sender)
    }

    pub async fn leave_channel(&self, name: String, agent: String) -> Result<(), ChannelError> {
        let channels = self.channel_map.lock().await;
        let mut agents = self.agent_task_map.lock().await;

        channels
            .get(&name)
            .ok_or(ChannelError::ChannelNotFound)?
            .leave(agent.clone())
            .await;

        match agents.entry(agent.clone()) {
            Entry::Occupied(mut o) => {
                let vecotr = o.get_mut();
                vecotr.retain(|task| {
                    if task.channel_name == name {
                        task.join_task.abort();
                    }
                    task.channel_name != name
                });
            }
            Entry::Vacant(_) => {}
        }
        Ok(())
    }
}

impl Default for ChannelControl {
    fn default() -> Self {
        Self::new()
    }
}

async fn channel_sub_to_agent(
    mut channel_sub_rx: broadcast::Receiver<ChannelMessage>,
    agent_tx: broadcast::Sender<ChannelMessage>,
) {
    while let Ok(channel_message) = channel_sub_rx.recv().await {
        match &channel_message {
            ChannelMessage::ReloadFilter { agent_id, code } => {
                info!("filter reloaded (do nothing)")
            }
            ChannelMessage::Reply(reply_message) => {
                let _ = agent_tx.send(channel_message);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use crate::channel::{Channel, ChannelControl, ChannelError, ChannelMessage};
    use crate::websocket::{ReplyMessage, ReplyPayload, Response};
    use tokio::sync::broadcast;

    fn create_test_message(topic: &str, reference: &str, message: &str) -> ChannelMessage {
        ChannelMessage::Reply(ReplyMessage {
            join_reference: None,
            reference: reference.to_string(),
            topic: topic.to_string(),
            event: "test_event".to_string(),
            payload: ReplyPayload {
                status: "ok".to_string(),
                response: Response::Message {
                    message: message.to_string(),
                },
            },
        })
    }

    #[tokio::test]
    async fn test_broadcast_capacity() {
        let capacity = 2;
        let (tx, mut rx1) = broadcast::channel::<&str>(capacity);
        let mut rx2 = tx.subscribe();

        tx.send("msg1").unwrap();
        tx.send("msg2").unwrap();
        tx.send("msg3").unwrap(); // the first message is discarded when the third message is sent, as it was never read
        tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;

        let mut r1_messages = Vec::new();
        while let Ok(msg) = rx1.try_recv() {
            r1_messages.push(msg);
        }

        let mut r2_messages = Vec::new();
        while let Ok(msg) = rx2.try_recv() {
            r2_messages.push(msg);
        }

        // FIXME: it's asserted, but it's not guaranteed that the message is lost
        assert!(
            !r1_messages.contains(&"msg1") || !r2_messages.contains(&"msg1"),
            "`msg1` is lost in one of them"
        );
    }

    // FIXME: test is flaky
    //
    // #[tokio::test]
    // async fn test_channel_capacity() {
    //     let channel = Channel::new("test".to_string(), Some(2));
    //     let agent_id = "agent1".to_string();
    //     let tx = channel.join(agent_id.clone()).await;
    //     let mut rx = tx.subscribe();
    //
    //     // Send messages with delay between each
    //     for i in 0..3 {
    //         let msg = create_test_message("test", &i.to_string(), &format!("msg{}", i));
    //         assert_eq!(channel.send(msg).unwrap(), 1);
    //         tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    //     }
    //
    //     // Give time for messages to propagate
    //     tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    //
    //     // Collect available messages
    //     let mut messages = Vec::new();
    //     while let Ok(msg) = rx.try_recv() {
    //         if let ChannelMessage::Reply(reply) = msg {
    //             if let Response::Message { message } = reply.payload.response {
    //                 messages.push(message);
    //             }
    //         }
    //     }
    //
    //     // With lagged receiver, we should only get the last 2 messages due to capacity limit
    //     assert_eq!(messages.len(), 2);
    //     assert!(messages.contains(&"msg1".to_string()));
    //     assert!(messages.contains(&"msg2".to_string()));
    // }

    #[tokio::test]
    async fn test_channel_creation_and_basic_ops() {
        let channel = Channel::new("test".to_string(), None);
        assert_eq!(channel.name, "test");
        assert!(channel.empty());

        // Test joining
        let agent_id = "agent1".to_string();
        let _tx = channel.join(agent_id.clone()).await;
        assert!(!channel.empty());

        // Test agent count
        assert_eq!(channel.agents().await.len(), 1);

        // Test duplicate join
        let _tx2 = channel.join(agent_id.clone()).await;
        assert_eq!(channel.agents().await.len(), 1); // Should not increase

        // Test leave
        channel.leave(agent_id).await;
        assert!(channel.empty());
    }

    #[tokio::test]
    async fn test_channel_message_broadcast() {
        let channel = Channel::new("test".to_string(), Some(10));
        let agent_id = "agent1".to_string();

        // Join and get sender
        let tx = channel.join(agent_id.clone()).await;
        let mut rx = tx.subscribe();

        // Test message sending
        let test_msg = create_test_message("test", "1", "hello");
        let recv_count = channel.send(test_msg.clone()).unwrap();
        assert_eq!(recv_count, 1);

        // Verify received message
        if let Ok(ChannelMessage::Reply(msg)) = rx.try_recv() {
            assert_eq!(msg.topic, "test");
            if let Response::Message { message } = msg.payload.response {
                assert_eq!(message, "hello");
            } else {
                panic!("Wrong response type");
            }
        } else {
            panic!("Failed to receive message");
        }
    }

    // FIXME: Test is flaky
    //
    // #[tokio::test]
    // async fn test_channel_control_operations() {
    //     let ctl = ChannelControl::new();
    //
    //     // Test channel management
    //     ctl.new_channel("room1".into(), None).await;
    //     ctl.new_channel("room2".into(), None).await;
    //
    //     // Test agent operations
    //     ctl.add_agent("user1".into(), None).await;
    //     ctl.add_agent("user2".into(), None).await;
    //
    //     // Test joining
    //     assert!(ctl.join_channel("room1", "user1".into()).await.is_ok());
    //     assert!(ctl.join_channel("room2", "user1".into()).await.is_ok());
    //     assert!(ctl.join_channel("room1", "user2".into()).await.is_ok());
    //
    //     // Test broadcasting
    //     let msg = create_test_message("room1", "1", "hello room1");
    //     let recv_count = ctl.broadcast("room1".into(), msg).await.unwrap();
    //     assert_eq!(recv_count, 2); // Both users should receive
    //
    //     // Test leaving
    //     assert!(ctl
    //         .leave_channel("room1".into(), "user1".into())
    //         .await
    //         .is_ok());
    //     let msg = create_test_message("room1", "2", "hello again");
    //     let recv_count = ctl.broadcast("room1".into(), msg).await.unwrap();
    //     assert_eq!(recv_count, 1); // Only user2 should receive
    // }

    #[tokio::test]
    async fn test_channel_error_cases() {
        let ctl = ChannelControl::new();

        // Test non-existent channel
        let result = ctl.join_channel("nonexistent", "user1".into()).await;
        assert!(matches!(result.unwrap_err(), ChannelError::ChannelNotFound));

        // Test non-initiated agent
        ctl.new_channel("room1".into(), None).await;
        let result = ctl.join_channel("room1", "user1".into()).await;
        assert!(matches!(
            result.unwrap_err(),
            ChannelError::AgentNotInitiated
        ));

        // Test leave non-existent channel
        let result = ctl
            .leave_channel("nonexistent".into(), "user1".into())
            .await;
        assert!(matches!(result.unwrap_err(), ChannelError::ChannelNotFound));
    }

    #[tokio::test]
    async fn test_agent_subscription() {
        let ctl = ChannelControl::new();

        // Setup channels and agent
        ctl.new_channel("room1".into(), None).await;
        ctl.add_agent("user1".into(), None).await;

        // Test subscription before join
        let sub = ctl.get_agent_subscription("user1".into()).await;
        assert!(sub.is_ok());

        // Join channel and test broadcasting
        ctl.join_channel("room1", "user1".into()).await.unwrap();
        let msg = create_test_message("room1", "1", "test");
        let count = ctl.broadcast("room1".into(), msg).await.unwrap();
        assert_eq!(count, 1);

        // Test subscription after removal
        ctl.remove_agent("user1".into()).await;
        let sub = ctl.get_agent_subscription("user1".into()).await;
        assert!(matches!(sub.unwrap_err(), ChannelError::AgentNotInitiated));
    }

    #[tokio::test]
    async fn test_ctl_add_remove() {
        let ctl = ChannelControl::new();
        assert_eq!(ctl.channel_map.lock().await.len(), 0);
        // assert!(ctl.empty().await, "Should have no channels");

        ctl.new_channel("test".into(), None).await;
        assert_eq!(ctl.channel_map.lock().await.len(), 1);

        ctl.remove_channel("test".into()).await;
        assert_eq!(ctl.channel_map.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn test_join_leave() {
        let ctl = ChannelControl::new();

        ctl.new_channel("test".into(), None).await; // new channel

        // new agent
        let agent_id = "agent1".to_string();
        ctl.add_agent(agent_id.clone(), None).await;

        // join channel
        let result = ctl.join_channel("test", agent_id.clone()).await;
        assert!(result.is_ok(), "Should successfully join channel");

        // leave channel
        let result = ctl
            .leave_channel("test".to_string(), agent_id.clone())
            .await;
        assert!(result.is_ok(), "Should successfully leave channel");
    }

    #[tokio::test]
    async fn test_channel_basics() {
        let ctl = ChannelControl::new();

        // new channel
        ctl.new_channel("test".into(), None).await;

        // new agent
        let agent_id = "agent1".to_string();
        ctl.add_agent(agent_id.clone(), None).await;

        // join channel
        let result = ctl.join_channel("test", agent_id.clone()).await;
        assert!(result.is_ok(), "Should successfully join channel");

        // broadcast message
        let message = ChannelMessage::Reply(ReplyMessage {
            join_reference: None,
            reference: "1".to_string(),
            topic: "test".to_string(),
            event: "test_event".to_string(),
            payload: crate::websocket::ReplyPayload {
                status: "ok".to_string(),
                response: crate::websocket::Response::Message {
                    message: "test message".to_string(),
                },
            },
        });

        let result = ctl.broadcast("test".to_string(), message).await;
        assert!(result.is_ok(), "Should successfully broadcast message");
        assert_eq!(result.unwrap(), 1, "Should have 1 receiver");

        // leave channel
        let result = ctl
            .leave_channel("test".to_string(), agent_id.clone())
            .await;
        assert!(result.is_ok(), "Should successfully leave channel");
    }

    #[tokio::test]
    async fn test_multiple_agents() {
        let ctl = ChannelControl::new();
        ctl.new_channel("room1".into(), None).await;

        // Add multiple agents
        let agent_ids = vec!["agent1", "agent2", "agent3"];
        for agent_id in &agent_ids {
            ctl.add_agent(agent_id.to_string(), None).await;
            let result = ctl.join_channel("room1", agent_id.to_string()).await;
            assert!(result.is_ok(), "Agent should join successfully");
        }

        // Broadcast a message
        let message = ChannelMessage::Reply(ReplyMessage {
            join_reference: None,
            reference: "1".to_string(),
            topic: "room1".to_string(),
            event: "broadcast".to_string(),
            payload: crate::websocket::ReplyPayload {
                status: "ok".to_string(),
                response: crate::websocket::Response::Message {
                    message: "hello all".to_string(),
                },
            },
        });

        let result = ctl.broadcast("room1".to_string(), message).await;
        assert!(result.is_ok(), "Should successfully broadcast");
        assert_eq!(result.unwrap(), 3, "Should have 3 receivers");
    }
}
