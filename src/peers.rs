use anyhow::{bail, Context, Result};
use event_listener::EventListener;
use futures_util::{
    future::{select, Either},
    stream::StreamExt,
};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tokio::{spawn, sync::RwLock};
use tracing::{debug, trace, warn};
use zbus::{
    connection::socket::BoxedSplit,
    message,
    names::{BusName, OwnedUniqueName, UniqueName},
    zvariant::Optional,
    AuthMechanism, Message, OwnedGuid,
};

use crate::{
    fdo,
    match_rules::MatchRules,
    name_registry::{NameOwnerChanged, NameRegistry},
    peer::{Monitor, Peer, Stream},
};

#[derive(Debug)]
pub struct Peers {
    peers: RwLock<BTreeMap<OwnedUniqueName, Peer>>,
    monitors: RwLock<BTreeMap<OwnedUniqueName, Monitor>>,
    name_registry: RwLock<NameRegistry>,
}

impl Peers {
    pub fn new() -> Arc<Self> {
        let name_registry = NameRegistry::default();

        Arc::new(Self {
            peers: RwLock::new(BTreeMap::new()),
            monitors: RwLock::new(BTreeMap::new()),
            name_registry: RwLock::new(name_registry),
        })
    }

    pub async fn add(
        self: &Arc<Self>,
        guid: &OwnedGuid,
        id: usize,
        socket: BoxedSplit,
        auth_mechanism: AuthMechanism,
    ) -> Result<()> {
        // Perform SASL handshake without holding the peers lock, as it involves async
        // I/O and would block all other peer operations (Hello handling, etc.).
        let (peer, peer_stream) = Peer::new(guid.clone(), id, socket, auth_mechanism).await?;
        let unique_name = peer.unique_name().clone();
        let listener = peer.listen_cancellation();

        let mut peers = self.peers_mut().await;
        match peers.get(&unique_name) {
            Some(peer) => panic!(
                "Unique name `{}` re-used. We're in deep trouble if this happens",
                peer.unique_name()
            ),
            None => {
                tokio::spawn(
                    self.clone()
                        .serve_peer(peer_stream, listener, unique_name.clone()),
                );
                peers.insert(unique_name.clone(), peer);
            }
        }

        Ok(())
    }

    pub async fn add_us(self: &Arc<Self>, conn: zbus::Connection) {
        let (peer, peer_stream) = Peer::new_us(conn).await;
        let unique_name = peer.unique_name().clone();
        let listener = peer.listen_cancellation();

        let mut peers = self.peers_mut().await;
        match peers.get(&unique_name) {
            Some(peer) => panic!(
                "Unique name `{}` re-used. We're in deep trouble if this happens",
                peer.unique_name()
            ),
            None => {
                tokio::spawn(
                    self.clone()
                        .serve_peer(peer_stream, listener, unique_name.clone()),
                );
                peers.insert(unique_name.clone(), peer);
            }
        }
    }

    pub async fn peers(&self) -> impl Deref<Target = BTreeMap<OwnedUniqueName, Peer>> + '_ {
        self.peers.read().await
    }

    pub async fn peers_mut(&self) -> impl DerefMut<Target = BTreeMap<OwnedUniqueName, Peer>> + '_ {
        self.peers.write().await
    }

    pub async fn name_registry(&self) -> impl Deref<Target = NameRegistry> + '_ {
        self.name_registry.read().await
    }

    pub async fn name_registry_mut(&self) -> impl DerefMut<Target = NameRegistry> + '_ {
        self.name_registry.write().await
    }

    pub async fn make_monitor(
        self: &Arc<Self>,
        peer_name: &UniqueName<'_>,
        match_rules: MatchRules,
    ) -> bool {
        let monitor = {
            let mut peers = self.peers_mut().await;
            let peer = match peers.remove(peer_name.as_str()) {
                Some(peer) => peer,
                None => {
                    return false;
                }
            };

            peer.become_monitor(match_rules)
        };

        let monitor_monitoring_fut = monitor.monitor();
        let unique_name = monitor.unique_name().clone();
        let peers = self.clone();
        self.monitors
            .write()
            .await
            .insert(unique_name.clone(), monitor);

        spawn(async move {
            monitor_monitoring_fut.await;
            peers.monitors.write().await.remove(&unique_name);
            debug!("Monitor {} disconnected", unique_name);
        });

        true
    }

    pub async fn notify_name_changes(&self, name_owner_changed: NameOwnerChanged) -> Result<()> {
        let name = BusName::from(name_owner_changed.name);
        let old_owner = name_owner_changed.old_owner.map(UniqueName::from);
        let new_owner = name_owner_changed.new_owner.map(UniqueName::from);

        // First broadcast the name change signal.
        let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameOwnerChanged")
            .unwrap()
            .sender(fdo::BUS_NAME)
            .unwrap()
            .build(&(
                &name,
                Optional::from(old_owner.clone()),
                Optional::from(new_owner.clone()),
            ))?;
        self.broadcast_msg(msg).await;

        // Now unicast the appropriate signal to the old and new owners.
        if let Some(old_owner) = old_owner {
            let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameLost")
                .unwrap()
                .sender(fdo::BUS_NAME)
                .unwrap()
                .destination(old_owner.clone())
                .unwrap()
                .build(&name)?;
            if let Err(e) = self.send_msg_to_unique_name(msg, old_owner.clone()).await {
                warn!("Couldn't notify inexistant peer {old_owner} about loosing name {name}: {e}")
            }
        }
        if let Some(new_owner) = new_owner {
            let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameAcquired")
                .unwrap()
                .sender(fdo::BUS_NAME)
                .unwrap()
                .destination(new_owner.clone())
                .unwrap()
                .build(&name)?;
            if let Err(e) = self.send_msg_to_unique_name(msg, new_owner.clone()).await {
                warn!("Couldn't notify peer {new_owner} about acquiring name {name}: {e}")
            }
        }

        Ok(())
    }

    async fn serve_peer(
        self: Arc<Self>,
        mut peer_stream: Stream,
        mut cancellation_listener: EventListener,
        unique_name: OwnedUniqueName,
    ) -> Result<()> {
        loop {
            let msg = match select(cancellation_listener, peer_stream.next()).await {
                Either::Left(_) | Either::Right((None, _)) => {
                    trace!("Peer `{}` disconnected", unique_name);

                    break;
                }
                Either::Right((Some(msg), listener)) => {
                    cancellation_listener = listener;

                    match msg {
                        Ok(msg) => msg,
                        Err(e) => {
                            debug!("{e}");

                            continue;
                        }
                    }
                }
            };

            match msg.message_type() {
                message::Type::Signal => self.broadcast_msg(msg).await,
                _ => match msg.header().destination() {
                    Some(dest) => {
                        if let Err(e) = self.send_msg(msg.clone(), dest.clone()).await {
                            warn!("{}", e);
                        }
                    }
                    // peer::Stream ensures a valid destination so this isn't exactly needed.
                    _ => bail!("invalid message: {:?}", msg),
                },
            };
        }

        // Stream is done means the peer disconnected or it became a monitor. Remove it from the
        // list of peers.
        if self.peers_mut().await.remove(&unique_name).is_none() {
            // This means peer was turned into a monitor. `Monitoring` iface will emit the signals.
            return Ok(());
        }
        let names_changes = self
            .name_registry_mut()
            .await
            .release_all(unique_name.inner().clone())
            .await;
        for changed in names_changes {
            self.notify_name_changes(changed).await?;
        }
        self.notify_name_changes(NameOwnerChanged {
            name: BusName::from(unique_name.clone()).into(),
            old_owner: Some(unique_name.clone()),
            new_owner: None,
        })
        .await?;

        Ok(())
    }

    async fn send_msg(&self, msg: Message, destination: BusName<'_>) -> Result<()> {
        trace!(
            "Forwarding message: {:?}, destination: {}",
            msg,
            destination
        );
        match destination {
            BusName::Unique(dest) => self.send_msg_to_unique_name(msg, dest.clone()).await,
            BusName::WellKnown(name) => {
                let dest = match self.name_registry().await.lookup(name.clone()) {
                    Some(dest) => dest,
                    None => bail!("unknown destination: {}", name),
                };
                self.send_msg_to_unique_name(msg, (&*dest).into()).await
            }
        }
    }

    async fn send_msg_to_unique_name(
        &self,
        msg: Message,
        destination: UniqueName<'_>,
    ) -> Result<()> {
        let conn = self
            .peers
            .read()
            .await
            .get(destination.as_str())
            .map(|peer| peer.conn().clone());
        match conn {
            Some(conn) => conn.send(&msg).await.context("failed to send message")?,
            None => debug!("no peer for destination `{destination}`"),
        }
        let name_registry = self.name_registry().await;
        self.broadcast_to_monitors(msg, &name_registry).await;

        Ok(())
    }

    async fn broadcast_msg(&self, msg: Message) {
        trace!("Broadcasting message: {:?}", msg);
        let name_registry = self.name_registry().await;
        for peer in self.peers.read().await.values() {
            if !peer.interested(&msg, &name_registry) {
                trace!("Peer {} not interested in {msg:?}", peer.unique_name());
                continue;
            }

            if let Err(e) = peer
                .conn()
                .send(&msg)
                .await
                .context("failed to send message")
            {
                warn!("Error sending message: {}", e);
            }
        }

        self.broadcast_to_monitors(msg, &name_registry).await;
    }

    async fn broadcast_to_monitors(&self, msg: Message, name_registry: &NameRegistry) {
        let monitors = self.monitors.read().await;
        if monitors.is_empty() {
            return;
        }
        trace!(
            "Broadcasting message to {} monitors: {:?}",
            monitors.len(),
            msg
        );
        for monitor in monitors.values() {
            if !monitor.interested(&msg, name_registry) {
                trace!(
                    "Monitor {} not interested in {msg:?}",
                    monitor.unique_name()
                );
                continue;
            }

            if let Err(e) = monitor
                .conn()
                .send(&msg)
                .await
                .context("failed to send message")
            {
                warn!("Error sending message: {}", e);
            }
        }
    }
}
