//! The `org.freedesktop.DBus` interface.

use std::{collections::HashMap, sync::Arc};

use zbus::{
    fdo::{ConnectionCredentials, Error, RequestNameFlags, Result, StartServiceReply},
    message::{self, Header},
    names::{BusName, MemberName, OwnedBusName, OwnedUniqueName, UniqueName, WellKnownName},
    Message, OwnedMatchRule,
};

use super::{args, failed, unknown_method, Answer, Driver, BUS_NAME, MONITORING, PATH, STATS};
use crate::{info, name_registry::NameOwnerChanged, peers::Peers, warn};

impl Driver {
    /// Answer the call of `member` of the `org.freedesktop.DBus` interface, `call`, whose header
    /// is `header`.
    pub(super) async fn answer_dbus(
        &mut self,
        peers: &Peers,
        call: &Message,
        header: &Header<'_>,
        member: &MemberName<'_>,
    ) -> Result<Answer> {
        let body = call.body();
        let sender = sender(header)?;
        match member.as_str() {
            "Hello" => peers.hello(header, sender).await,
            "RequestName" => {
                let (name, flags) = args(&body)?;

                peers.request_name(header, sender, name, flags).await
            }
            "ReleaseName" => peers.release_name(header, sender, args(&body)?).await,
            "GetNameOwner" => {
                let owner = peers.name_owner(args(&body)?).await?;

                Answer::reply(header, &owner)
            }
            "AddMatch" => {
                let rule: OwnedMatchRule = args(&body)?;
                peers.routing_mut().await.add_match_rule(sender, rule)?;

                Answer::reply(header, &())
            }
            "RemoveMatch" => {
                let rule: OwnedMatchRule = args(&body)?;
                peers.routing_mut().await.remove_match_rule(sender, rule)?;

                Answer::reply(header, &())
            }
            "GetAdtAuditSessionData" => {
                let _: BusName<'_> = args(&body)?;

                Err(Error::NotSupported("Solaris really?".to_string()))
            }
            "GetConnectionCredentials" => {
                let creds = peers.credentials(args(&body)?).await?;

                Answer::reply(header, &*creds)
            }
            "GetConnectionSELinuxSecurityContext" => {
                let creds = peers.credentials(args(&body)?).await?;
                let Some(label) = creds.linux_security_label() else {
                    return Err(Error::SELinuxSecurityContextUnknown(
                        "Unimplemented".to_string(),
                    ));
                };

                Answer::reply(header, label)
            }
            "GetConnectionUnixProcessID" => {
                let name: BusName<'_> = args(&body)?;
                let creds = peers.credentials(name.clone()).await?;
                let Some(pid) = creds.process_id() else {
                    return Err(Error::UnixProcessIdUnknown(format!(
                        "Could not determine Unix process ID of `{name}`"
                    )));
                };

                Answer::reply(header, &pid)
            }
            "GetConnectionUnixUser" => {
                let name: BusName<'_> = args(&body)?;
                let creds = peers.credentials(name.clone()).await?;
                let Some(uid) = creds.unix_user_id() else {
                    return Err(Error::Failed(format!(
                        "Could not determine Unix user ID of `{name}`"
                    )));
                };

                Answer::reply(header, &uid)
            }
            "GetId" => Answer::reply(header, &self.guid),
            // The bus starts no services, so no name can be activated.
            "ListActivatableNames" => Answer::reply(header, &Vec::<OwnedBusName>::new()),
            "ListNames" => Answer::reply(header, &peers.names().await),
            "ListQueuedOwners" => {
                let owners = peers.queued_owners(args(&body)?).await?;

                Answer::reply(header, &owners)
            }
            "NameHasOwner" => {
                let has_owner = peers.has_owner(args(&body)?).await?;

                Answer::reply(header, &has_owner)
            }
            "StartServiceByName" => {
                let (name, _flags): (WellKnownName<'_>, u32) = args(&body)?;
                if peers.has_owner(name.into()).await? {
                    return Answer::reply(header, &StartServiceReply::AlreadyRunning);
                }

                Err(Error::Failed(
                    "Service activation not yet supported".to_string(),
                ))
            }
            "UpdateActivationEnvironment" => {
                let _: HashMap<&str, &str> = args(&body)?;
                peers.ensure_privileged(sender).await?;

                Err(Error::Failed(
                    "Service activation not yet supported".to_string(),
                ))
            }
            "ReloadConfig" => {
                peers.reload_config().await?;

                Answer::reply(header, &())
            }
            "Dune" => Answer::reply(header, &DUNE),
            _ => Err(unknown_method(member)),
        }
    }
}

/// The signals of the name owner changes `changes`, in order.
///
/// For each change, `NameOwnerChanged` goes to all, then `NameLost` to the old owner, if any, and
/// `NameAcquired` to the new owner, if any.
pub(crate) fn name_change_signals<'c, C>(changes: C) -> zbus::Result<Vec<Message>>
where
    C: IntoIterator<Item = &'c NameOwnerChanged>,
{
    let mut signals = Vec::new();
    for changed in changes {
        let name = BusName::from(changed.name());
        let old_owner = changed.old_owner().map(UniqueName::from);
        let new_owner = changed.new_owner().map(UniqueName::from);
        let owner = |owner: &Option<UniqueName<'_>>| {
            owner.as_ref().map_or_else(String::new, ToString::to_string)
        };
        signals.push(signal("NameOwnerChanged", None).build(&(
            &name,
            owner(&old_owner),
            owner(&new_owner),
        ))?);
        if let Some(old_owner) = &old_owner {
            signals.push(signal("NameLost", Some(old_owner)).build(&name)?);
        }
        if let Some(new_owner) = &new_owner {
            signals.push(signal("NameAcquired", Some(new_owner)).build(&name)?);
        }
    }

    Ok(signals)
}

/// The interfaces of the object of the bus that its `Interfaces` property lists, which leave out
/// the interfaces that every bus has, as the specification says.
pub(super) const INTERFACES: [&str; 2] = [MONITORING, STATS];

/// The sender of the call of `header`, which the bus sets on each message of its clients.
fn sender<'h>(header: &'h Header<'_>) -> Result<&'h UniqueName<'h>> {
    header
        .sender()
        .ok_or_else(|| Error::Failed("The call has no sender".to_string()))
}

impl Peers {
    /// Answer `Hello` of `sender`, whose header is `header`: it can be called once.
    ///
    /// The reply comes before the signals of the peer's unique name, so that the peer has its name
    /// by the time that it gets them, as with dbus-broker.
    async fn hello(&self, header: &Header<'_>, sender: &UniqueName<'_>) -> Result<Answer> {
        let unique_name = {
            let mut routing = self.routing_mut().await;
            let peer = routing.peer_mut(sender.as_str())?;
            peer.hello()?;

            peer.client().unique_name().clone()
        };
        let reply = super::reply(header, &unique_name)?;
        let after =
            name_change_signals([&NameOwnerChanged::appeared(unique_name)]).map_err(failed)?;

        Ok(Answer {
            before: Vec::new(),
            reply,
            after,
        })
    }

    /// Answer `RequestName` of `sender`, whose header is `header`, for `name`, with `flags`.
    ///
    /// The signals of the change of the owner of the name come before the reply.
    async fn request_name(
        &self,
        header: &Header<'_>,
        sender: &UniqueName<'_>,
        name: WellKnownName<'_>,
        flags: RequestNameFlags,
    ) -> Result<Answer> {
        ensure_not_reserved(&name)?;
        let (reply, changed) = {
            let mut routing = self.routing_mut().await;
            let peer = routing.peer(sender.as_str())?;
            if !peer.policy().may_own(&name) {
                warn!(
                    "Refusing `{name}` to `{sender}`, as the policy does not let it own the name"
                );

                return Err(Error::AccessDenied(
                    "Request to own name refused by policy".to_string(),
                ));
            }

            routing
                .names_mut()
                .request_name(name, sender.clone(), flags)
        };

        Ok(Answer {
            before: name_change_signals(&changed).map_err(failed)?,
            reply: super::reply(header, &reply)?,
            after: Vec::new(),
        })
    }

    /// Answer `ReleaseName` of `sender`, whose header is `header`, for `name`.
    ///
    /// The signals of the change of the owner of the name come before the reply.
    async fn release_name(
        &self,
        header: &Header<'_>,
        sender: &UniqueName<'_>,
        name: WellKnownName<'_>,
    ) -> Result<Answer> {
        ensure_not_reserved(&name)?;
        let (reply, changed) = self
            .routing_mut()
            .await
            .names_mut()
            .release_name(name, sender.clone());

        Ok(Answer {
            before: name_change_signals(&changed).map_err(failed)?,
            reply: super::reply(header, &reply)?,
            after: Vec::new(),
        })
    }

    /// Whether `name` has an owner.
    async fn has_owner(&self, name: BusName<'_>) -> Result<bool> {
        match self.name_owner(name).await {
            Ok(_) => Ok(true),
            Err(Error::NameHasNoOwner(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The credentials that the bus has of the owner of `name`: those of the peer's process, or
    /// those of the bus's own process.
    async fn credentials(&self, name: BusName<'_>) -> Result<Arc<ConnectionCredentials>> {
        let owner = self.name_owner(name.clone()).await?;
        if owner == BUS_NAME {
            return Ok(Arc::new(own_credentials()));
        }
        // Each peer has the credentials that the bus looked up when its client connected, or when
        // the bus took the client over.
        self.routing()
            .await
            .peer(&owner)
            .map(|peer| peer.credentials().clone())
            .map_err(|_| Error::Failed(format!("Peer `{name}` not found")))
    }

    /// The unique name of the owner of `name`.
    ///
    /// For a well-known name, the owner is its primary owner. For a unique name, it is the peer of
    /// that name, or the bus itself.
    async fn name_owner(&self, name: BusName<'_>) -> Result<OwnedUniqueName> {
        let routing = self.routing().await;
        match name {
            BusName::WellKnown(name) => routing.names().lookup(&name).cloned().ok_or_else(|| {
                Error::NameHasNoOwner("Name is not owned by anyone. Take it!".to_string())
            }),
            BusName::Unique(name) if name == BUS_NAME || routing.has_peer(&name) => Ok(name.into()),
            BusName::Unique(_) => Err(Error::NameHasNoOwner(
                "Name is not owned by anyone.".to_string(),
            )),
        }
    }

    /// The names that have an owner: the unique names of the bus and of its peers, and the
    /// well-known names that are owned.
    async fn names(&self) -> Vec<OwnedBusName> {
        let routing = self.routing().await;
        let bus = BusName::Unique(UniqueName::from_static_str_unchecked(BUS_NAME));
        let unique_names = routing.peer_names().map(BusName::from);
        let well_known_names = routing.names().all_names().keys().map(BusName::from);

        [bus]
            .into_iter()
            .chain(unique_names)
            .chain(well_known_names)
            .map(OwnedBusName::from)
            .collect()
    }

    /// The peers that queue for `name`, in order.
    async fn queued_owners(&self, name: WellKnownName<'_>) -> Result<Vec<OwnedUniqueName>> {
        self.routing()
            .await
            .names()
            .waiting_list(name)
            .ok_or_else(|| {
                Error::NameHasNoOwner("Name is not owned by anyone. Take it!".to_string())
            })
            .map(|owners| owners.map(|owner| owner.unique_name()).cloned().collect())
    }

    /// Read the configuration of the bus again, and go by its policy.
    ///
    /// If that fails, the bus goes on by the policy that it has, and the call fails, as with
    /// dbus-broker: the reason is for the log alone. The policy is read off the thread of the bus,
    /// which forwards the messages of its peers meanwhile. The calls to the bus that come in before
    /// the reload is done wait for it, as they wait for any call before them.
    async fn reload_config(&self) -> Result<()> {
        match self.reload().await {
            Some(Ok(())) => {
                info!("Reloaded the configuration.");

                Ok(())
            }
            Some(Err(e)) => {
                warn!("Failed to reload the configuration, which stays as it is: {e:#}");

                Err(Error::Failed("Config invalid. Reload ignored.".to_string()))
            }
            None => Err(Error::Failed(
                "No server configuration to reload.".to_string(),
            )),
        }
    }
}

/// Fails for the name of the bus itself, which no client may ask for or let go of, as with
/// dbus-broker and the reference implementation.
fn ensure_not_reserved(name: &WellKnownName<'_>) -> Result<()> {
    if name == BUS_NAME {
        return Err(Error::InvalidArgs(format!("{BUS_NAME} is a reserved name")));
    }

    Ok(())
}

/// The credentials of the process of the bus: its pid, and its effective user and group.
fn own_credentials() -> ConnectionCredentials {
    ConnectionCredentials::default()
        .set_process_id(std::process::id())
        .set_unix_user_id(rustix::process::geteuid().as_raw())
        .add_unix_group_id(rustix::process::getegid().as_raw())
}

/// A signal of the bus, from its object, as `member` of its interface, to `destination` if any,
/// and to the peers that subscribed to it otherwise.
fn signal<'s>(member: &'s str, destination: Option<&'s str>) -> message::Builder<'s> {
    let builder = Message::signal(PATH, BUS_NAME, member).sender(BUS_NAME);

    match destination {
        Some(destination) => builder.destination(destination),
        None => builder,
    }
}

/// The answer of the easter egg method, `Dune`.
const DUNE: &str =
    "I must not fear. Fear is the mind-killer. Fear is the little-death that brings total \
     obliteration. I will face my fear. I will permit it to pass over me and through me. And when \
     it has gone past I will turn the inner eye to see its path. Where the fear has gone there \
     will be nothing. Only **I** will remain!";
