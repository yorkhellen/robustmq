use super::{packet::MQTTAckBuild, session::save_connect_session, subscribe::send_retain_message};
use crate::{
    cluster::heartbeat_manager::{ConnectionLiveTime, HeartbeatManager},
    metadata::{cache::MetadataCache, message::Message, session::LastWillData, topic::Topic},
    security::authentication::authentication_login,
    server::tcp::packet::ResponsePackage,
    storage::{message::MessageStorage, topic::TopicStorage},
    subscribe::manager::SubScribeManager,
};
use common_base::{
    errors::RobustMQError,
    log::error,
    tools::{now_second, unique_id},
};
use protocol::mqtt::{
    Connect, ConnectProperties, Disconnect, DisconnectProperties, DisconnectReasonCode, LastWill,
    LastWillProperties, Login, MQTTPacket, PingReq, PubAck, PubAckProperties, PubRel,
    PubRelProperties, Publish, PublishProperties, QoS, Subscribe, SubscribeProperties, Unsubscribe,
    UnsubscribeProperties,
};
use std::sync::Arc;
use storage_adapter::storage::{ShardConfig, StorageAdapter};
use tokio::sync::broadcast::Sender;

#[derive(Clone)]
pub struct Mqtt5Service<T, S> {
    metadata_cache: Arc<MetadataCache<T>>,
    subscribe_manager: Arc<SubScribeManager<T>>,
    ack_build: MQTTAckBuild<T>,
    heartbeat_manager: Arc<HeartbeatManager>,
    metadata_storage_adapter: Arc<T>,
    message_storage_adapter: Arc<S>,
}

impl<T, S> Mqtt5Service<T, S>
where
    T: StorageAdapter + Sync + Send + 'static + Clone,
    S: StorageAdapter + Sync + Send + 'static + Clone,
{
    pub fn new(
        metadata_cache: Arc<MetadataCache<T>>,
        subscribe_manager: Arc<SubScribeManager<T>>,
        ack_build: MQTTAckBuild<T>,
        heartbeat_manager: Arc<HeartbeatManager>,
        metadata_storage_adapter: Arc<T>,
        message_storage_adapter: Arc<S>,
    ) -> Self {
        return Mqtt5Service {
            metadata_cache,
            subscribe_manager,
            ack_build,
            heartbeat_manager,
            metadata_storage_adapter,
            message_storage_adapter,
        };
    }

    pub async fn connect(
        &mut self,
        connect_id: u64,
        connnect: Connect,
        connect_properties: Option<ConnectProperties>,
        last_will: Option<LastWill>,
        last_will_properties: Option<LastWillProperties>,
        login: Option<Login>,
    ) -> MQTTPacket {
        let cluster = self.metadata_cache.get_cluster_info();
        // connect for authentication
        match authentication_login(
            self.metadata_cache.clone(),
            &cluster,
            login,
            &connect_properties,
        )
        .await
        {
            Ok(flag) => {
                if !flag {
                    return self
                        .ack_build
                        .distinct(DisconnectReasonCode::NotAuthorized, None);
                }
            }
            Err(e) => {
                return self
                    .ack_build
                    .distinct(DisconnectReasonCode::NotAuthorized, Some(e.to_string()));
            }
        }

        // auto create client id
        let client_id = if connnect.client_id.is_empty() {
            unique_id()
        } else {
            connnect.client_id.clone()
        };

        // save session data
        let client_session = match save_connect_session(
            client_id.clone(),
            !last_will.is_none(),
            &cluster,
            &connnect,
            &connect_properties,
            self.metadata_storage_adapter.clone(),
        )
        .await
        {
            Ok(session) => session,
            Err(e) => {
                error(e.to_string());
                return self.ack_build.distinct(
                    DisconnectReasonCode::AdministrativeAction,
                    Some(e.to_string()),
                );
            }
        };

        // save last will data
        if !last_will.is_none() {
            let last_will = LastWillData {
                last_will,
                last_will_properties,
            };

            let message_storage = MessageStorage::new(self.message_storage_adapter.clone());
            match message_storage
                .save_lastwill(client_id.clone(), last_will)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    error(e.to_string());
                    return self
                        .ack_build
                        .distinct(DisconnectReasonCode::UnspecifiedError, Some(e.to_string()));
                }
            }
        }

        // update cache
        // When working with locks, the default is to release them as soon as possible
        self.metadata_cache
            .set_session(client_id.clone(), client_session.clone());
        self.metadata_cache
            .set_client_id(connect_id, client_id.clone());

        // Record heartbeat information
        let session_keep_alive = client_session.keep_alive;
        let live_time: ConnectionLiveTime = ConnectionLiveTime {
            protobol: crate::server::MQTTProtocol::MQTT5,
            keep_live: session_keep_alive,
            heartbeat: now_second(),
        };
        self.heartbeat_manager
            .report_hearbeat(connect_id, live_time);

        return self
            .ack_build
            .conn_ack(
                &cluster,
                &client_session,
                client_id,
                connnect.client_id.is_empty(),
            )
            .await;
    }

    pub async fn publish(
        &self,
        connect_id: u64,
        publish: Publish,
        publish_properties: Option<PublishProperties>,
    ) -> Option<MQTTPacket> {
        let topic_name = String::from_utf8(publish.topic.to_vec()).unwrap();
        let client_id = if let Some(se) = self.metadata_cache.connect_id_info.get(&connect_id) {
            se.clone()
        } else {
            return Some(self.ack_build.distinct(
                DisconnectReasonCode::UnspecifiedError,
                Some(RobustMQError::NotFoundConnectionInCache(connect_id).to_string()),
            ));
        };

        let session = if let Some(se) = self.metadata_cache.session_info.get(&client_id) {
            se.clone()
        } else {
            return Some(self.ack_build.distinct(
                DisconnectReasonCode::UnspecifiedError,
                Some(RobustMQError::NotFoundClientInCache(client_id).to_string()),
            ));
        };

        let topic = if let Some(tp) = self.metadata_cache.get_topic_by_name(topic_name.clone()) {
            tp
        } else {
            // Persisting the topic information
            let topic = Topic::new(&topic_name);
            self.metadata_cache.set_topic(&topic_name, &topic);
            let topic_storage = TopicStorage::new(self.metadata_storage_adapter.clone());
            match topic_storage.save_topic(&topic_name, &topic).await {
                Ok(_) => {}
                Err(e) => {
                    error(e.to_string());
                    return Some(
                        self.ack_build
                            .distinct(DisconnectReasonCode::UnspecifiedError, Some(e.to_string())),
                    );
                }
            }

            // Create the resource object of the storage layer
            let shard_name = topic.topic_id.clone();
            let shard_config = ShardConfig::default();
            match self
                .message_storage_adapter
                .create_shard(shard_name, shard_config)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    error(e.to_string());
                    return Some(
                        self.ack_build
                            .distinct(DisconnectReasonCode::UnspecifiedError, Some(e.to_string())),
                    );
                }
            }
            topic
        };

        // Persisting retain message data
        let message_storage = MessageStorage::new(self.message_storage_adapter.clone());
        if publish.retain {
            let retain_message =
                Message::build_message(publish.clone(), publish_properties.clone());
            match message_storage
                .save_retain_message(topic.topic_id.clone(), retain_message)
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    error(e.to_string());
                    return Some(
                        self.ack_build
                            .distinct(DisconnectReasonCode::UnspecifiedError, Some(e.to_string())),
                    );
                }
            }
        }

        // Persisting stores message data
        let mut offset = "".to_string();
        if let Some(record) = Message::build_record(publish.clone(), publish_properties.clone()) {
            match message_storage
                .append_topic_message(topic.topic_id, vec![record])
                .await
            {
                Ok(da) => {
                    offset = format!("{:?}", da);
                }
                Err(e) => {
                    error(e.to_string());
                    return Some(
                        self.ack_build
                            .distinct(DisconnectReasonCode::UnspecifiedError, Some(e.to_string())),
                    );
                }
            }
        }

        // Pub Ack information is built
        let pkid = publish.pkid;
        let mut user_properties = Vec::new();
        if let Some(properties) = publish_properties {
            user_properties = properties.user_properties;
        }
        if !offset.is_empty() {
            user_properties.push(("offset".to_string(), offset));
        }

        //ontent is returned according to different QOS levels
        match publish.qos {
            QoS::AtMostOnce => {
                return None;
            }
            QoS::AtLeastOnce => {
                return Some(self.ack_build.pub_ack(pkid, None, user_properties));
            }
            QoS::ExactlyOnce => {
                return Some(self.ack_build.pub_rec(session.session_present));
            }
        }
    }

    pub fn publish_ack(&self, pub_ack: PubAck, puback_properties: Option<PubAckProperties>) {}

    pub fn publish_rel(&self, pub_rel: PubRel, pubrel_properties: Option<PubRelProperties>) {}

    pub async fn subscribe(
        &self,
        connect_id: u64,
        subscribe: Subscribe,
        subscribe_properties: Option<SubscribeProperties>,
        response_queue_sx: Sender<ResponsePackage>,
    ) -> MQTTPacket {
        // Saving subscriptions
        self.subscribe_manager
            .parse_subscribe(
                crate::server::MQTTProtocol::MQTT5,
                connect_id,
                subscribe.clone(),
                subscribe_properties.clone(),
            )
            .await;

        // Reservation messages are processed when a subscription is created
        let message_storage = MessageStorage::new(self.message_storage_adapter.clone());
        match send_retain_message(
            connect_id,
            subscribe.clone(),
            subscribe_properties.clone(),
            message_storage,
            self.metadata_cache.clone(),
            response_queue_sx.clone(),
            true,
            false,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                error(e.to_string());
                return self
                    .ack_build
                    .distinct(DisconnectReasonCode::UnspecifiedError, Some(e.to_string()));
            }
        }

        let pkid = subscribe.packet_identifier;
        let mut user_properties = Vec::new();
        if let Some(properties) = subscribe_properties {
            user_properties = properties.user_properties;
        }
        return self.ack_build.sub_ack(pkid, None, user_properties);
    }

    pub async fn ping(&self, connect_id: u64, _: PingReq) -> MQTTPacket {
        if let Some(client_id) = self.metadata_cache.connect_id_info.get(&connect_id) {
            if let Some(session_info) = self.metadata_cache.session_info.get(&client_id.clone()) {
                let live_time = ConnectionLiveTime {
                    protobol: crate::server::MQTTProtocol::MQTT5,
                    keep_live: session_info.keep_alive,
                    heartbeat: now_second(),
                };
                self.heartbeat_manager
                    .report_hearbeat(connect_id, live_time);
                return self.ack_build.ping_resp();
            }
        }
        return self
            .ack_build
            .distinct(DisconnectReasonCode::UseAnotherServer, None);
    }

    pub async fn un_subscribe(
        &self,
        connect_id: u64,
        un_subscribe: Unsubscribe,
        _: Option<UnsubscribeProperties>,
    ) -> MQTTPacket {
        // Remove subscription information
        if un_subscribe.filters.len() > 0 {
            let mut topic_ids = Vec::new();
            for topic_name in un_subscribe.filters {
                if let Some(topic) = self.metadata_cache.get_topic_by_name(topic_name) {
                    topic_ids.push(topic.topic_id);
                }
            }

            self.subscribe_manager
                .remove_subscribe(connect_id, topic_ids);
        }

        return self
            .ack_build
            .unsub_ack(un_subscribe.pkid, None, Vec::new());
    }

    pub async fn disconnect(
        &self,
        connect_id: u64,
        disconnect: Disconnect,
        disconnect_properties: Option<DisconnectProperties>,
    ) -> MQTTPacket {
        self.metadata_cache.remove_connect_id(connect_id);
        self.heartbeat_manager.remove_connect(connect_id);
        self.subscribe_manager.remove_connect_subscribe(connect_id);
        return self
            .ack_build
            .distinct(DisconnectReasonCode::NormalDisconnection, None);
    }
}
