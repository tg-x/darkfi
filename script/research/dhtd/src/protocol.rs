use async_executor::Executor;
use async_std::sync::Arc;
use async_trait::async_trait;
use chrono::Utc;
use log::{debug, error};

use darkfi::{
    net::{
        ChannelPtr, MessageSubscription, P2pPtr, ProtocolBase, ProtocolBasePtr,
        ProtocolJobsManager, ProtocolJobsManagerPtr,
    },
    Result,
};

use crate::structures::{KeyRequest, KeyResponse, StatePtr};

pub struct Protocol {
    channel: ChannelPtr,
    notify_queue_sender: async_channel::Sender<KeyResponse>,
    req_sub: MessageSubscription<KeyRequest>,
    resp_sub: MessageSubscription<KeyResponse>,
    jobsman: ProtocolJobsManagerPtr,
    state: StatePtr,
    p2p: P2pPtr,
}

impl Protocol {
    pub async fn init(
        channel: ChannelPtr,
        notify_queue_sender: async_channel::Sender<KeyResponse>,
        state: StatePtr,
        p2p: P2pPtr,
    ) -> Result<ProtocolBasePtr> {
        debug!("Adding Protocol to the protocol registry");
        let msg_subsystem = channel.get_message_subsystem();
        msg_subsystem.add_dispatch::<KeyRequest>().await;
        msg_subsystem.add_dispatch::<KeyResponse>().await;

        let req_sub = channel.subscribe_msg::<KeyRequest>().await?;
        let resp_sub = channel.subscribe_msg::<KeyResponse>().await?;

        Ok(Arc::new(Self {
            channel: channel.clone(),
            notify_queue_sender,
            req_sub,
            resp_sub,
            jobsman: ProtocolJobsManager::new("Protocol", channel),
            state,
            p2p,
        }))
    }

    async fn handle_receive_request(self: Arc<Self>) -> Result<()> {
        debug!("Protocol::handle_receive_request() [START]");
        let exclude_list = vec![self.channel.address()];
        loop {
            let req = match self.req_sub.receive().await {
                Ok(v) => v,
                Err(e) => {
                    error!("Protocol::handle_receive_request(): recv fail: {}", e);
                    continue
                }
            };

            let req_copy = (*req).clone();
            debug!("Protocol::handle_receive_request(): req: {:?}", req_copy);

            if self.state.read().await.seen.contains_key(&req_copy.id) {
                debug!("Protocol::handle_receive_request(): We have already seen this request.");
                continue
            }

            self.state.write().await.seen.insert(req_copy.id.clone(), Utc::now().timestamp());

            match self.state.read().await.map.get(&req_copy.key) {
                Some(value) => {
                    let response = KeyResponse::new(req_copy.daemon, req_copy.key, value.clone());
                    debug!("Protocol::handle_receive_request(): sending response: {:?}", response);
                    if let Err(e) = self.channel.send(response).await {
                        error!("Protocol::handle_receive_request(): p2p broadcast of response failed: {}", e);
                        continue
                    };
                }
                None => {
                    if let Err(e) = self.p2p.broadcast_with_exclude(req_copy, &exclude_list).await {
                        error!("Protocol::handle_receive_request(): p2p broadcast fail: {}", e);
                        continue
                    };
                }
            }
        }
    }

    async fn handle_receive_response(self: Arc<Self>) -> Result<()> {
        debug!("Protocol::handle_receive_response() [START]");
        let exclude_list = vec![self.channel.address()];
        loop {
            let resp = match self.resp_sub.receive().await {
                Ok(v) => v,
                Err(e) => {
                    error!("Protocol::handle_receive_response(): recv fail: {}", e);
                    continue
                }
            };

            let resp_copy = (*resp).clone();
            debug!("Protocol::handle_receive_response(): resp: {:?}", resp_copy);

            if self.state.read().await.seen.contains_key(&resp_copy.id) {
                debug!("Protocol::handle_receive_response(): We have already seen this response.");
                continue
            }

            self.state.write().await.seen.insert(resp_copy.id.clone(), Utc::now().timestamp());

            if self.state.read().await.id.to_string() != resp_copy.daemon {
                if let Err(e) =
                    self.p2p.broadcast_with_exclude(resp_copy.clone(), &exclude_list).await
                {
                    error!("Protocol::handle_receive_response(): p2p broadcast fail: {}", e);
                    continue
                };
            }

            self.notify_queue_sender.send(resp_copy).await?;
        }
    }
}

#[async_trait]
impl ProtocolBase for Protocol {
    async fn start(self: Arc<Self>, executor: Arc<Executor<'_>>) -> Result<()> {
        debug!("Protocol::start() [START]");
        self.jobsman.clone().start(executor.clone());
        self.jobsman.clone().spawn(self.clone().handle_receive_request(), executor.clone()).await;
        self.jobsman.clone().spawn(self.clone().handle_receive_response(), executor.clone()).await;
        debug!("Protocol::start() [END]");
        Ok(())
    }

    fn name(&self) -> &'static str {
        "Protocol"
    }
}
