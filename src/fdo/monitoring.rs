use std::sync::{Arc, Weak};

use crate::{debug, warn};
use tokio::spawn;
use zbus::{
    fdo::{Error, Result},
    interface, message,
    object_server::{ResponseDispatchNotifier, SignalEmitter},
    Optional,
};

use super::msg_sender;
use crate::{fdo::DBus, match_rules::MatchRules, peers::Peers};

#[derive(Debug)]
pub struct Monitoring {
    peers: Weak<Peers>,
}

impl Monitoring {
    pub const PATH: &'static str = "/org/freedesktop/DBus";
    pub const INTERFACE: &'static str = "org.freedesktop.DBus.Monitoring";

    pub fn new(peers: Arc<Peers>) -> Self {
        Self {
            peers: Arc::downgrade(&peers),
        }
    }
}

#[interface(
    interface = "org.freedesktop.DBus.Monitoring",
    introspection_docs = false
)]
impl Monitoring {
    async fn become_monitor(
        &self,
        match_rules: MatchRules,
        _flags: u32,
        #[zbus(header)] hdr: message::Header<'_>,
        #[zbus(signal_emitter)] ctxt: SignalEmitter<'_>,
    ) -> Result<ResponseDispatchNotifier<()>> {
        let owner = msg_sender(&hdr).to_owned();
        let peers = self
            .peers
            .upgrade()
            // Can it happen in any other situation than the bus shutting down?
            .ok_or_else(|| Error::Failed("Bus shutting down.".to_string()))?;
        if !peers.make_monitor(&owner, match_rules).await {
            return Err(Error::NameHasNoOwner(format!("No such peer: {owner}")));
        }
        debug!("{} became a monitor", owner);

        // We want to emit the name change signals **after** the `BecomeMonitor` method returns.
        // Otherwise, some clients (e.g `busctl monitor`) can get confused.
        let (response, listener) = ResponseDispatchNotifier::new(());
        let ctxt = ctxt.to_owned();
        spawn(async move {
            listener.await;

            let names_changes = peers
                .name_registry_mut()
                .await
                .release_all(owner.clone())
                .await;
            for changed in names_changes {
                if let Err(e) = DBus::name_owner_changed(
                    &ctxt,
                    changed.name.clone().into(),
                    Some(owner.clone()).into(),
                    Optional::default(),
                )
                .await
                {
                    warn!("Failed to notify peers of name change: {}", e);
                }

                let ctxt = ctxt.clone().set_destination(owner.clone().into());
                if let Err(e) = DBus::name_lost(&ctxt, changed.name.into()).await {
                    warn!("Failed to send `NameLost` signal: {}", e);
                }
            }

            if let Err(e) = DBus::name_owner_changed(
                &ctxt,
                owner.clone().into(),
                Some(owner.clone()).into(),
                None.into(),
            )
            .await
            {
                warn!("Failed to notify peers of name change: {}", e);
            }

            let ctxt = ctxt.set_destination(owner.clone().into());
            if let Err(e) = DBus::name_lost(&ctxt, owner.into()).await {
                warn!("Failed to send `NameLost` signal: {}", e);
            }
        });

        Ok(response)
    }
}
