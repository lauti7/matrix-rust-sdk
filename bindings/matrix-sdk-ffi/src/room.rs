use std::{
    convert::TryFrom,
    sync::{Arc, RwLock},
};

use anyhow::{bail, Context, Result};
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    room::Room as MatrixRoom,
    ruma::{
        events::room::message::{RoomMessageEvent, RoomMessageEventContent},
        EventId, UserId,
    },
};

use super::{
    backward_stream::BackwardsStream,
    messages::{sync_event_to_message, AnyMessage},
    RUNTIME,
};

pub trait RoomDelegate: Sync + Send {
    fn did_receive_message(&self, messages: Arc<AnyMessage>);
}

pub enum Membership {
    Invited,
    Joined,
    Left,
}

pub struct Room {
    room: MatrixRoom,
    delegate: Arc<RwLock<Option<Box<dyn RoomDelegate>>>>,
    is_listening_to_live_events: Arc<RwLock<bool>>,
}

impl Room {
    pub fn new(room: MatrixRoom) -> Self {
        Room {
            room,
            delegate: Arc::new(RwLock::new(None)),
            is_listening_to_live_events: Arc::new(RwLock::new(false)),
        }
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn RoomDelegate>>) {
        *self.delegate.write().unwrap() = delegate;
    }

    pub fn id(&self) -> String {
        self.room.room_id().to_string()
    }

    pub fn name(&self) -> Option<String> {
        self.room.name()
    }

    pub fn display_name(&self) -> Result<String> {
        let r = self.room.clone();
        RUNTIME.block_on(async move { Ok(r.display_name().await?.to_string()) })
    }

    pub fn topic(&self) -> Option<String> {
        self.room.topic()
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.room.avatar_url().map(|m| m.to_string())
    }

    pub fn member_avatar_url(&self, user_id: String) -> Result<Option<String>> {
        let room = self.room.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = <&UserId>::try_from(&*user_id).expect("Invalid user id.");
            let member = room.get_member(user_id).await?.expect("No user found");
            let avatar_url_string = member.avatar_url().map(|m| m.to_string());
            Ok(avatar_url_string)
        })
    }

    pub fn member_display_name(&self, user_id: String) -> Result<Option<String>> {
        let room = self.room.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = <&UserId>::try_from(&*user_id).expect("Invalid user id.");
            let member = room.get_member(user_id).await?.expect("No user found");
            let avatar_url_string = member.display_name().map(|m| m.to_owned());
            Ok(avatar_url_string)
        })
    }

    pub fn membership(&self) -> Membership {
        match &self.room {
            MatrixRoom::Invited(_) => Membership::Invited,
            MatrixRoom::Joined(_) => Membership::Joined,
            MatrixRoom::Left(_) => Membership::Left,
        }
    }

    pub fn is_direct(&self) -> bool {
        self.room.is_direct()
    }

    pub fn is_public(&self) -> bool {
        self.room.is_public()
    }

    pub fn is_encrypted(&self) -> bool {
        self.room.is_encrypted()
    }

    pub fn is_space(&self) -> bool {
        self.room.is_space()
    }

    pub fn is_tombstoned(&self) -> bool {
        self.room.is_tombstoned()
    }

    pub fn start_live_event_listener(&self) -> Option<Arc<BackwardsStream>> {
        if *self.is_listening_to_live_events.read().unwrap() {
            return None;
        }

        *self.is_listening_to_live_events.write().unwrap() = true;

        let room = self.room.clone();
        let delegate = self.delegate.clone();
        let is_listening_to_live_events = self.is_listening_to_live_events.clone();

        let (forward_stream, backwards) = RUNTIME.block_on(async move {
            room.timeline().await.expect("Failed acquiring timeline streams")
        });

        RUNTIME.spawn(async move {
            pin_mut!(forward_stream);

            while let Some(sync_event) = forward_stream.next().await {
                if !(*is_listening_to_live_events.read().unwrap()) {
                    return;
                }

                if let Some(delegate) = &*delegate.read().unwrap() {
                    if let Some(message) = sync_event_to_message(sync_event) {
                        delegate.did_receive_message(message)
                    }
                }
            }
        });
        Some(Arc::new(BackwardsStream::new(Box::pin(backwards))))
    }

    pub fn stop_live_event_listener(&self) {
        *self.is_listening_to_live_events.write().unwrap() = false;
    }

    pub fn send(&self, msg: Arc<RoomMessageEventContent>, txn_id: Option<String>) -> Result<()> {
        let room = match &self.room {
            MatrixRoom::Joined(j) => j.clone(),
            _ => bail!("Can't send to a room that isn't in joined state"),
        };

        RUNTIME.block_on(async move {
            room.send((*msg).to_owned(), txn_id.as_deref().map(Into::into)).await?;
            Ok(())
        })
    }

    pub fn send_reply(
        &self,
        msg: String,
        in_reply_to_event_id: String,
        txn_id: Option<String>,
    ) -> Result<()> {
        let room = match &self.room {
            MatrixRoom::Joined(j) => j.clone(),
            _ => bail!("Can't send to a room that isn't in joined state"),
        };

        let event_id: &EventId =
            in_reply_to_event_id.as_str().try_into().context("Failed to create EventId.")?;

        RUNTIME.block_on(async move {
            let timeline_event = room.event(event_id).await.context("Couldn't find event.")?;

            let event_content = timeline_event
                .event
                .deserialize_as::<RoomMessageEvent>()
                .context("Couldn't deserialise event")?;

            let original_message =
                event_content.as_original().context("Couldn't retrieve original message.")?;

            let reply_content =
                RoomMessageEventContent::text_markdown(msg).make_reply_to(original_message);

            room.send(reply_content, txn_id.as_deref().map(Into::into)).await?;

            Ok(())
        })
    }

    /// Redacts an event from the room.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to redact
    ///
    /// * `reason` - The reason for the event being redacted (optional).
    ///
    /// * `txn_id` - A unique ID that can be attached to this event as
    /// its transaction ID (optional). If not given one is created.
    pub fn redact(
        &self,
        event_id: String,
        reason: Option<String>,
        txn_id: Option<String>,
    ) -> Result<()> {
        let room = match &self.room {
            MatrixRoom::Joined(j) => j.clone(),
            _ => bail!("Can't redact in a room that isn't in joined state"),
        };

        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
            room.redact(&event_id, reason.as_deref(), txn_id.map(Into::into)).await?;
            Ok(())
        })
    }
}

impl std::ops::Deref for Room {
    type Target = MatrixRoom;
    fn deref(&self) -> &MatrixRoom {
        &self.room
    }
}
