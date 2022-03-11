// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The state store holds the overall state for rooms, users and their
//! profiles and their timelines. It is an overall cache for faster access
//! and convenience- accessible through `Store`.
//!
//! Implementing the `StateStore` trait, you can plug any storage backend
//! into the store for the actual storage. By default this brings an in-memory
//! store.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
    pin::Pin,
    result::Result as StdResult,
    sync::Arc,
};

#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;

use dashmap::DashMap;
use matrix_sdk_common::{async_trait, locks::RwLock, AsyncTraitDeps};
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::store::CryptoStore;
use ruma::{
    api::client::push::get_notifications::v3::Notification,
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptEventContent},
        room::member::RoomMemberEventContent,
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent, EventContent, GlobalAccountDataEventType, RoomAccountDataEventType,
        StateEventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    EventId, MxcUri, RoomId, UserId,
};

#[cfg(feature = "store_key")]
pub mod store_key;

/// BoxStream of owned Types
pub type BoxStream<T> = Pin<Box<dyn futures_util::Stream<Item = T> + Send>>;

use crate::{
    deserialized_responses::{MemberEvent, StrippedMemberEvent, SyncRoomEvent, TimelineSlice},
    media::MediaRequest,
    rooms::{RoomInfo, RoomType},
    Room, Session,
};

pub(crate) mod ambiguity_map;
mod memory_store;

pub use self::memory_store::MemoryStore;

/// State store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    /// An error happened in the underlying database backend.
    Backend(#[from] anyhow::Error),
    /// An error happened while serializing or deserializing some data.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// An error happened while deserializing a Matrix identifier, e.g. an user
    /// id.
    #[error(transparent)]
    Identifier(#[from] ruma::identifiers::Error),
    /// The store is locked with a passphrase and an incorrect passphrase was
    /// given.
    #[error("The store failed to be unlocked")]
    StoreLocked,
    /// An unencrypted store was tried to be unlocked with a passphrase.
    #[error("The store is not encrypted but was tried to be opened with a passphrase")]
    UnencryptedStore,
    /// The store failed to encrypt or decrypt some data.
    #[error("Error encrypting or decrypting data from the store: {0}")]
    Encryption(String),
    /// The store failed to encode or decode some data.
    #[error("Error encoding or decoding data from the store: {0}")]
    Codec(String),
    /// Redacting an event in the store has failed.
    ///
    /// This should never happen.
    #[error("Redaction failed: {0}")]
    Redaction(#[source] ruma::signatures::Error),
}
/// A `StateStore` specific result type.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// An abstract state store trait that can be used to implement different stores
/// for the SDK.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait StateStore: AsyncTraitDeps {
    /// Save the given filter id under the given name.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name that should be used to store the filter id.
    ///
    /// * `filter_id` - The filter id that should be stored in the state store.
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()>;

    /// Save the set of state changes in the store.
    async fn save_changes(&self, changes: &StateChanges) -> Result<()>;

    /// Get the filter id that was stored under the given filter name.
    ///
    /// # Arguments
    ///
    /// * `filter_name` - The name that was used to store the filter id.
    async fn get_filter(&self, filter_name: &str) -> Result<Option<String>>;

    /// Get the last stored sync token.
    async fn get_sync_token(&self) -> Result<Option<String>>;

    /// Get the stored presence event for the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the user for which we wish to fetch the presence
    /// event for.
    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>>;

    /// Get a state event out of the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room the state event was received for.
    ///
    /// * `event_type` - The event type of the state event.
    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>>;

    /// Get a list of state events for a given room and `StateEventType`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room to find events for.
    ///
    /// * `event_type` - The event type.
    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>>;

    /// Get the current profile for the given user in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id the profile is used in.
    ///
    /// * `user_id` - The id of the user the profile belongs to.
    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<RoomMemberEventContent>>;

    /// Get a raw `MemberEvent` for the given state key in the given room id.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id the member event belongs to.
    ///
    /// * `state_key` - The user id that the member event defines the state for.
    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>>;

    /// Get all the user ids of members for a given room.
    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>>;

    /// Get all the user ids of members that are in the invited state for a
    /// given room.
    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>>;

    /// Get all the user ids of members that are in the joined state for a
    /// given room.
    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<Box<UserId>>>;

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>>;

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>>;

    /// Get all the users that use the given display name in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the display name users should
    /// be fetched for.
    ///
    /// * `display_name` - The display name that the users use.
    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<Box<UserId>>>;

    /// Get an event out of the account data store.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The event type of the account data event.
    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>>;

    /// Get an event out of the room account data store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the room account data event
    ///   should
    /// be fetched.
    ///
    /// * `event_type` - The event type of the room account data event.
    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>>;

    /// Get an event out of the user room receipt store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the receipt should be
    ///   fetched.
    ///
    /// * `receipt_type` - The type of the receipt.
    ///
    /// * `user_id` - The id of the user for who the receipt should be fetched.
    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(Box<EventId>, Receipt)>>;

    /// Get events out of the event room receipt store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the receipts should be
    ///   fetched.
    ///
    /// * `receipt_type` - The type of the receipts.
    ///
    /// * `event_id` - The id of the event for which the receipts should be
    ///   fetched.
    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(Box<UserId>, Receipt)>>;

    /// Get arbitrary data from the custom store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to fetch data for
    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Put arbitrary data into the custom store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert data into
    ///
    /// * `value` - The value to insert
    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>>;

    /// Add a media file's content in the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    ///
    /// * `content` - The content of the file.
    async fn add_media_content(&self, request: &MediaRequest, content: Vec<u8>) -> Result<()>;

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>>;

    /// Removes a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()>;

    /// Removes all the media files' content associated to an `MxcUri` from the
    /// media store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media files.
    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()>;

    /// Removes a room and all elements associated from the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to delete.
    async fn remove_room(&self, room_id: &RoomId) -> Result<()>;

    /// Get a stream of the stored timeline
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to delete.
    ///
    /// Returns a stream of events and a token that can be used to request
    /// previous events.
    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<(BoxStream<Result<SyncRoomEvent>>, Option<String>)>>;
}

/// A state store wrapper for the SDK.
///
/// This adds additional higher level store functionality on top of a
/// `StateStore` implementation.
#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<dyn StateStore>,
    pub(crate) session: Arc<RwLock<Option<Session>>>,
    pub(crate) sync_token: Arc<RwLock<Option<String>>>,
    rooms: Arc<DashMap<Box<RoomId>, Room>>,
    stripped_rooms: Arc<DashMap<Box<RoomId>, Room>>,
}

impl Store {
    /// Create a new Store with the default `MemoryStore`
    pub fn open_memory_store() -> Self {
        let inner = Box::new(MemoryStore::new());

        Self::new(inner)
    }
}

impl Store {
    /// Create a new store, wrappning the given `StateStore`
    pub fn new(inner: Box<dyn StateStore>) -> Self {
        Self {
            inner: inner.into(),
            session: Default::default(),
            sync_token: Default::default(),
            rooms: Default::default(),
            stripped_rooms: Default::default(),
        }
    }

    /// Restore the access to the Store from the given `Session`, overwrites any
    /// previously existing access to the Store.
    pub async fn restore_session(&self, session: Session) -> Result<()> {
        for info in self.inner.get_room_infos().await? {
            let room = Room::restore(&session.user_id, self.inner.clone(), info);
            self.rooms.insert(room.room_id().to_owned(), room);
        }

        for info in self.inner.get_stripped_room_infos().await? {
            let room = Room::restore(&session.user_id, self.inner.clone(), info);
            self.stripped_rooms.insert(room.room_id().to_owned(), room);
        }

        let token = self.get_sync_token().await?;

        *self.sync_token.write().await = token;
        *self.session.write().await = Some(session);

        Ok(())
    }

    /// Get all the rooms this store knows about.
    pub fn get_rooms(&self) -> Vec<Room> {
        self.rooms.iter().filter_map(|r| self.get_room(r.key())).collect()
    }

    /// Get the room with the given room id.
    pub fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.rooms
            .get(room_id)
            .and_then(|r| match r.room_type() {
                RoomType::Joined => Some(r.clone()),
                RoomType::Left => Some(r.clone()),
                RoomType::Invited => self.get_stripped_room(room_id),
            })
            .or_else(|| self.get_stripped_room(room_id))
    }

    /// Get all the rooms this store knows about.
    pub fn get_stripped_rooms(&self) -> Vec<Room> {
        self.stripped_rooms.iter().filter_map(|r| self.get_stripped_room(r.key())).collect()
    }

    /// Get the stripped room with the given room id.
    pub fn get_stripped_room(&self, room_id: &RoomId) -> Option<Room> {
        self.stripped_rooms.get(room_id).map(|r| r.clone())
    }

    /// Lookup the stripped Room for the given RoomId, or create one, if it
    /// didn't exist yet in the store
    pub async fn get_or_create_stripped_room(&self, room_id: &RoomId) -> Room {
        let session = self.session.read().await;
        let user_id = &session.as_ref().expect("Creating room while not being logged in").user_id;

        self.stripped_rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| Room::new(user_id, self.inner.clone(), room_id, RoomType::Invited))
            .clone()
    }

    /// Lookup the Room for the given RoomId, or create one, if it didn't exist
    /// yet in the store
    pub async fn get_or_create_room(&self, room_id: &RoomId, room_type: RoomType) -> Room {
        if room_type == RoomType::Invited {
            return self.get_or_create_stripped_room(room_id).await;
        }

        let session = self.session.read().await;
        let user_id = &session.as_ref().expect("Creating room while not being logged in").user_id;

        self.rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| Room::new(user_id, self.inner.clone(), room_id, room_type))
            .clone()
    }
}

impl Deref for Store {
    type Target = dyn StateStore;

    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}

/// Store state changes and pass them to the StateStore.
#[derive(Debug, Default)]
pub struct StateChanges {
    /// The sync token that relates to this update.
    pub sync_token: Option<String>,
    /// A user session, containing an access token and information about the
    /// associated user account.
    pub session: Option<Session>,
    /// A mapping of event type string to `AnyBasicEvent`.
    pub account_data: BTreeMap<String, Raw<AnyGlobalAccountDataEvent>>,
    /// A mapping of `UserId` to `PresenceEvent`.
    pub presence: BTreeMap<Box<UserId>, Raw<PresenceEvent>>,

    /// A mapping of `RoomId` to a map of users and their `MemberEvent`.
    pub members: BTreeMap<Box<RoomId>, BTreeMap<Box<UserId>, MemberEvent>>,
    /// A mapping of `RoomId` to a map of users and their
    /// `RoomMemberEventContent`.
    pub profiles: BTreeMap<Box<RoomId>, BTreeMap<Box<UserId>, RoomMemberEventContent>>,

    /// A mapping of `RoomId` to a map of event type string to a state key and
    /// `AnySyncStateEvent`.
    #[allow(clippy::type_complexity)]
    pub state: BTreeMap<Box<RoomId>, BTreeMap<String, BTreeMap<String, Raw<AnySyncStateEvent>>>>,
    /// A mapping of `RoomId` to a map of event type string to `AnyBasicEvent`.
    pub room_account_data: BTreeMap<Box<RoomId>, BTreeMap<String, Raw<AnyRoomAccountDataEvent>>>,
    /// A map of `RoomId` to `RoomInfo`.
    pub room_infos: BTreeMap<Box<RoomId>, RoomInfo>,
    /// A map of `RoomId` to `ReceiptEventContent`.
    pub receipts: BTreeMap<Box<RoomId>, ReceiptEventContent>,

    /// A mapping of `RoomId` to a map of event type to a map of state key to
    /// `AnyStrippedStateEvent`.
    #[allow(clippy::type_complexity)]
    pub stripped_state:
        BTreeMap<Box<RoomId>, BTreeMap<String, BTreeMap<String, Raw<AnyStrippedStateEvent>>>>,
    /// A mapping of `RoomId` to a map of users and their `StrippedMemberEvent`.
    pub stripped_members: BTreeMap<Box<RoomId>, BTreeMap<Box<UserId>, StrippedMemberEvent>>,
    /// A map of `RoomId` to `RoomInfo` for stripped rooms (e.g. for invites or
    /// while knocking)
    pub stripped_room_infos: BTreeMap<Box<RoomId>, RoomInfo>,

    /// A map from room id to a map of a display name and a set of user ids that
    /// share that display name in the given room.
    pub ambiguity_maps: BTreeMap<Box<RoomId>, BTreeMap<String, BTreeSet<Box<UserId>>>>,
    /// A map of `RoomId` to a vector of `Notification`s
    pub notifications: BTreeMap<Box<RoomId>, Vec<Notification>>,
    /// A mapping of `RoomId` to a `TimelineSlice`
    pub timeline: BTreeMap<Box<RoomId>, TimelineSlice>,
}

impl StateChanges {
    /// Create a new `StateChanges` struct with the given sync_token.
    pub fn new(sync_token: String) -> Self {
        Self { sync_token: Some(sync_token), ..Default::default() }
    }

    /// Update the `StateChanges` struct with the given `PresenceEvent`.
    pub fn add_presence_event(&mut self, event: PresenceEvent, raw_event: Raw<PresenceEvent>) {
        self.presence.insert(event.sender, raw_event);
    }

    /// Update the `StateChanges` struct with the given `RoomInfo`.
    pub fn add_room(&mut self, room: RoomInfo) {
        self.room_infos.insert(room.room_id.as_ref().to_owned(), room);
    }

    /// Update the `StateChanges` struct with the given `RoomInfo`.
    pub fn add_stripped_room(&mut self, room: RoomInfo) {
        self.stripped_room_infos.insert(room.room_id.as_ref().to_owned(), room);
    }

    /// Update the `StateChanges` struct with the given `AnyBasicEvent`.
    pub fn add_account_data(
        &mut self,
        event: AnyGlobalAccountDataEvent,
        raw_event: Raw<AnyGlobalAccountDataEvent>,
    ) {
        self.account_data.insert(event.content().event_type().to_owned(), raw_event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `AnyBasicEvent`.
    pub fn add_room_account_data(
        &mut self,
        room_id: &RoomId,
        event: AnyRoomAccountDataEvent,
        raw_event: Raw<AnyRoomAccountDataEvent>,
    ) {
        self.room_account_data
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.content().event_type().to_owned(), raw_event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `StrippedMemberEvent`.
    pub fn add_stripped_member(&mut self, room_id: &RoomId, event: StrippedMemberEvent) {
        let user_id = event.state_key.clone();

        self.stripped_members
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(user_id, event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `AnySyncStateEvent`.
    pub fn add_state_event(
        &mut self,
        room_id: &RoomId,
        event: AnySyncStateEvent,
        raw_event: Raw<AnySyncStateEvent>,
    ) {
        self.state
            .entry(room_id.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(event.content().event_type().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(event.state_key().to_owned(), raw_event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `Notification`.
    pub fn add_notification(&mut self, room_id: &RoomId, notification: Notification) {
        self.notifications.entry(room_id.to_owned()).or_insert_with(Vec::new).push(notification);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `Receipts`.
    pub fn add_receipts(&mut self, room_id: &RoomId, event: ReceiptEventContent) {
        self.receipts.insert(room_id.to_owned(), event);
    }

    /// Update the `StateChanges` struct with the given room with a new
    /// `TimelineSlice`.
    pub fn add_timeline(&mut self, room_id: &RoomId, timeline: TimelineSlice) {
        self.timeline.insert(room_id.to_owned(), timeline);
    }
}

/// Configuration for the state store and, when `encryption` is enabled, for the
/// crypto store.
///
/// # Example
///
/// ```
/// # use matrix_sdk_base::store::StoreConfig;
///
/// let store_config = StoreConfig::new();
/// ```
#[derive(Default)]
pub struct StoreConfig {
    #[cfg(feature = "encryption")]
    pub(crate) crypto_store: Option<Box<dyn CryptoStore>>,
    pub(crate) state_store: Option<Box<dyn StateStore>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        fmt.debug_struct("StoreConfig").finish()
    }
}

impl StoreConfig {
    /// Create a new default `StoreConfig`.
    #[must_use]
    pub fn new() -> Self {
        Default::default()
    }

    /// Set a custom implementation of a `CryptoStore`.
    ///
    /// The crypto store must be opened before being set.
    #[cfg(feature = "encryption")]
    pub fn crypto_store(mut self, store: Box<dyn CryptoStore>) -> Self {
        self.crypto_store = Some(store);
        self
    }

    /// Set a custom implementation of a `StateStore`.
    pub fn state_store(mut self, store: Box<dyn StateStore>) -> Self {
        self.state_store = Some(store);
        self
    }
}
