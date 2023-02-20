use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex, RwLock as StdRwLock},
};

use derive_builder::Builder;
use futures_signals::signal::Mutable;
use ruma::{
    api::client::sync::sync_events::v4::{
        self, AccountDataConfig, E2EEConfig, ExtensionsConfig, ReceiptConfig, ToDeviceConfig,
        TypingConfig,
    },
    assign, OwnedRoomId,
};
use tracing::trace;
use url::Url;

use super::{
    Error, FrozenSlidingSync, FrozenSlidingSyncView, SlidingSync, SlidingSyncRoom, SlidingSyncView,
    SlidingSyncViewBuilder,
};
use crate::{Client, Result};

/// Configuration for a Sliding Sync Instance
#[derive(Clone, Debug, Builder)]
#[builder(
    public,
    name = "SlidingSyncBuilder",
    pattern = "owned",
    build_fn(name = "build_no_cache", private),
    derive(Clone, Debug)
)]
pub(super) struct SlidingSyncConfig {
    /// The storage key to keep this cache at and load it from
    #[builder(setter(strip_option), default)]
    storage_key: Option<String>,
    /// Customize the homeserver for sliding sync only
    #[builder(setter(strip_option), default)]
    homeserver: Option<Url>,
    /// The client this sliding sync will be using
    client: Client,
    /// Views.
    #[builder(private, default)]
    views: BTreeMap<String, SlidingSyncView>,
    /// Extensions.
    #[builder(private, default)]
    extensions: Option<ExtensionsConfig>,
    /// Subscriptions.
    #[builder(default)]
    subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
}

impl SlidingSyncConfig {
    pub async fn build(self) -> Result<SlidingSync> {
        let SlidingSyncConfig {
            homeserver,
            storage_key,
            client,
            mut views,
            mut extensions,
            subscriptions,
        } = self;
        let mut delta_token_inner = None;
        let mut rooms_found: BTreeMap<OwnedRoomId, SlidingSyncRoom> = BTreeMap::new();

        if let Some(storage_key) = storage_key.as_ref() {
            trace!(storage_key, "trying to load from cold");

            for (name, view) in views.iter_mut() {
                if let Some(frozen_view) = client
                    .store()
                    .get_custom_value(format!("{storage_key}::{name}").as_bytes())
                    .await?
                    .map(|v| serde_json::from_slice::<FrozenSlidingSyncView>(&v))
                    .transpose()?
                {
                    trace!(name, "frozen for view found");

                    let FrozenSlidingSyncView { rooms_count, rooms_list, rooms } = frozen_view;
                    view.set_from_cold(rooms_count, rooms_list);
                    for (key, frozen_room) in rooms.into_iter() {
                        rooms_found.entry(key).or_insert_with(|| {
                            SlidingSyncRoom::from_frozen(frozen_room, client.clone())
                        });
                    }
                } else {
                    trace!(name, "no frozen state for view found");
                }
            }

            if let Some(FrozenSlidingSync { to_device_since, delta_token }) = client
                .store()
                .get_custom_value(storage_key.as_bytes())
                .await?
                .map(|v| serde_json::from_slice::<FrozenSlidingSync>(&v))
                .transpose()?
            {
                trace!("frozen for generic found");
                if let Some(since) = to_device_since {
                    if let Some(to_device_ext) =
                        extensions.get_or_insert_with(Default::default).to_device.as_mut()
                    {
                        to_device_ext.since = Some(since);
                    }
                }
                delta_token_inner = delta_token;
            }
            trace!("sync unfrozen done");
        };

        trace!(len = rooms_found.len(), "rooms unfrozen");
        let rooms = Arc::new(StdRwLock::new(rooms_found));
        let views = Arc::new(StdRwLock::new(views));

        Ok(SlidingSync {
            homeserver,
            client,
            storage_key,

            views,
            rooms,

            extensions: Mutex::new(extensions).into(),
            sent_extensions: Mutex::new(None).into(),
            failure_count: Default::default(),

            pos: Mutable::new(None),
            delta_token: Mutable::new(delta_token_inner),
            subscriptions: Arc::new(StdRwLock::new(subscriptions)),
            unsubscribe: Default::default(),
        })
    }
}

impl SlidingSyncBuilder {
    /// Convenience function to add a full-sync view to the builder
    pub fn add_fullsync_view(self) -> Self {
        self.add_view(
            SlidingSyncViewBuilder::default_with_fullsync()
                .build()
                .expect("Building default full sync view doesn't fail"),
        )
    }

    /// The cold cache key to read from and store the frozen state at
    pub fn cold_cache<T: ToString>(mut self, name: T) -> Self {
        self.storage_key = Some(Some(name.to_string()));
        self
    }

    /// Do not use the cold cache
    pub fn no_cold_cache(mut self) -> Self {
        self.storage_key = None;
        self
    }

    /// Reset the views to `None`
    pub fn no_views(mut self) -> Self {
        self.views = None;
        self
    }

    /// Add the given view to the views.
    ///
    /// Replace any view with the name.
    pub fn add_view(mut self, v: SlidingSyncView) -> Self {
        let views = self.views.get_or_insert_with(Default::default);
        views.insert(v.name.clone(), v);
        self
    }

    /// Activate e2ee, to-device-message and account data extensions if not yet
    /// configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_common_extensions(mut self) -> Self {
        {
            let mut cfg = self
                .extensions
                .get_or_insert_with(Default::default)
                .get_or_insert_with(Default::default);
            if cfg.to_device.is_none() {
                cfg.to_device = Some(assign!(ToDeviceConfig::default(), { enabled: Some(true) }));
            }

            if cfg.e2ee.is_none() {
                cfg.e2ee = Some(assign!(E2EEConfig::default(), { enabled: Some(true) }));
            }

            if cfg.account_data.is_none() {
                cfg.account_data =
                    Some(assign!(AccountDataConfig::default(), { enabled: Some(true) }));
            }
        }
        self
    }

    /// Activate e2ee, to-device-message, account data, typing and receipt
    /// extensions if not yet configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_all_extensions(mut self) -> Self {
        {
            let mut cfg = self
                .extensions
                .get_or_insert_with(Default::default)
                .get_or_insert_with(Default::default);
            if cfg.to_device.is_none() {
                cfg.to_device = Some(assign!(ToDeviceConfig::default(), { enabled: Some(true) }));
            }

            if cfg.e2ee.is_none() {
                cfg.e2ee = Some(assign!(E2EEConfig::default(), { enabled: Some(true) }));
            }

            if cfg.account_data.is_none() {
                cfg.account_data =
                    Some(assign!(AccountDataConfig::default(), { enabled: Some(true) }));
            }

            if cfg.receipt.is_none() {
                cfg.receipt = Some(assign!(ReceiptConfig::default(), { enabled: Some(true) }));
            }

            if cfg.typing.is_none() {
                cfg.typing = Some(assign!(TypingConfig::default(), { enabled: Some(true) }));
            }
        }
        self
    }

    /// Set the E2EE extension configuration.
    pub fn with_e2ee_extension(mut self, e2ee: E2EEConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .e2ee = Some(e2ee);
        self
    }

    /// Unset the E2EE extension configuration.
    pub fn without_e2ee_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .e2ee = None;
        self
    }

    /// Set the ToDevice extension configuration.
    pub fn with_to_device_extension(mut self, to_device: ToDeviceConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .to_device = Some(to_device);
        self
    }

    /// Unset the ToDevice extension configuration.
    pub fn without_to_device_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .to_device = None;
        self
    }

    /// Set the account data extension configuration.
    pub fn with_account_data_extension(mut self, account_data: AccountDataConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .account_data = Some(account_data);
        self
    }

    /// Unset the account data extension configuration.
    pub fn without_account_data_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .account_data = None;
        self
    }

    /// Set the Typing extension configuration.
    pub fn with_typing_extension(mut self, typing: TypingConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .typing = Some(typing);
        self
    }

    /// Unset the Typing extension configuration.
    pub fn without_typing_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .typing = None;
        self
    }

    /// Set the Receipt extension configuration.
    pub fn with_receipt_extension(mut self, receipt: ReceiptConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .receipt = Some(receipt);
        self
    }

    /// Unset the Receipt extension configuration.
    pub fn without_receipt_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .receipt = None;
        self
    }

    /// Build the Sliding Sync
    ///
    /// if configured, load the cached data from cold storage
    pub async fn build(self) -> Result<SlidingSync> {
        self.build_no_cache().map_err(Error::SlidingSyncBuilder)?.build().await
    }
}