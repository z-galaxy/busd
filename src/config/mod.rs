use std::{
    env::var,
    path::{Path, PathBuf},
    str::FromStr,
};

use crate::warn;
use anyhow::{Error, Result};
use policy::OptionalPolicy;
use serde::Deserialize;
use zbus::{address::Transport, Address};

pub mod policy;
pub mod rule;
mod xml;

pub use policy::Policy;
pub use rule::{
    Access, ConnectOperation, NameOwnership, Operation, ReceiveOperation, Rule, SendOperation,
};
use xml::{Document, Element, TypeElement};

/// The bus configuration.
///
/// This is currently only loaded from the [XML configuration files] defined by the specification.
/// We plan to add support for other formats (e.g JSON) in the future.
///
/// [XML configuration files]: https://dbus.freedesktop.org/doc/dbus-daemon.1.html#configuration_file
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Config {
    /// If `true`, the bus daemon becomes a real daemon (forks into the background, etc.).
    pub fork: bool,

    /// If `true`, the bus daemon keeps its original umask when forking.
    /// This may be useful to avoid affecting the behavior of child processes.
    pub keep_umask: bool,

    /// Address that the bus should listen on.
    /// The address is in the standard D-Bus format that contains a transport name plus possible
    /// parameters/options. The bus only listens on unix sockets, so it ignores the other
    /// addresses.
    // TODO: warn when multiple `<listen>` elements are defined, as we only support one
    // TODO: consider implementing `Deserialize` over in zbus crate, then removing this "skip..."
    #[serde(default, skip_deserializing)]
    pub listen: Option<Address>,

    /// The bus daemon will write its pid to the specified file.
    pub pidfile: Option<PathBuf>,

    pub policies: Vec<Policy>,

    /// Adds a directory to search for .service files,
    /// which tell the dbus-daemon how to start a program to provide a particular well-known bus
    /// name.
    #[serde(default)]
    pub servicedirs: Vec<PathBuf>,

    /// Specifies the setuid helper that is used to launch system daemons with an alternate user.
    pub servicehelper: Option<PathBuf>,

    /// If `true`, the bus daemon will log to syslog.
    pub syslog: bool,

    /// This element only controls which message bus specific environment variables are set in
    /// activated clients.
    pub r#type: Option<BusType>,

    /// The user account the daemon should run as, as either a username or a UID.
    /// If the daemon cannot change to this UID on startup, it will exit.
    /// If this element is not present, the daemon will not change or care about its UID.
    pub user: Option<String>,
}

impl TryFrom<Document> for Config {
    type Error = Error;

    fn try_from(value: Document) -> std::result::Result<Self, Self::Error> {
        let mut config = Config::default();

        for element in value.busconfig {
            match element {
                Element::AllowAnonymous | Element::Auth => {
                    // NO-OP: the bus only takes the EXTERNAL mechanism, as dbus-broker does
                }
                Element::Fork => config.fork = true,
                Element::Include(_) => {
                    // NO-OP: removed during `Document::resolve_includes`
                }
                Element::Includedir(_) => {
                    // NO-OP: removed during `Document::resolve_includedirs`
                }
                Element::KeepUmask => config.keep_umask = true,
                Element::Limit => {
                    // NO-OP: deprecated and ignored
                }
                Element::Listen(listen) => {
                    let address = Address::from_str(&listen)?;
                    match address.transport() {
                        Transport::Unix(_) => config.listen = Some(address),
                        _ => warn!("Ignoring `{listen}`: the bus only listens on unix sockets."),
                    }
                }
                Element::Pidfile(p) => config.pidfile = Some(p),
                Element::Policy(pe) => {
                    if let Some(p) = OptionalPolicy::try_from(pe)? {
                        config.policies.push(p);
                    }
                }
                Element::Servicedir(p) => {
                    config.servicedirs.push(p);
                }
                Element::Servicehelper(p) => {
                    // NOTE: we're assuming this has the same "last one wins" behaviour as `<type>`

                    // TODO: warn and then ignore if we aren't reading:
                    // /usr/share/dbus-1/system.conf
                    config.servicehelper = Some(p);
                }
                Element::StandardSessionServicedirs => {
                    // TODO: warn and then ignore if we aren't reading: /etc/dbus-1/session.conf
                    if let Ok(runtime_dir) = var("XDG_RUNTIME_DIR") {
                        config
                            .servicedirs
                            .push(PathBuf::from(runtime_dir).join("dbus-1/services"));
                    }
                    if let Ok(data_dir) = var("XDG_DATA_HOME") {
                        config
                            .servicedirs
                            .push(PathBuf::from(data_dir).join("dbus-1/services"));
                    }
                    let mut servicedirs_in_data_dirs = xdg_data_dirs()
                        .iter()
                        .map(|p| p.join("dbus-1/services"))
                        .collect();
                    config.servicedirs.append(&mut servicedirs_in_data_dirs);
                    config
                        .servicedirs
                        .push(PathBuf::from("/usr/share/dbus-1/services"));
                }
                Element::StandardSystemServicedirs => {
                    // TODO: warn and then ignore if we aren't reading:
                    // /usr/share/dbus-1/system.conf
                    config
                        .servicedirs
                        .extend(STANDARD_SYSTEM_SERVICEDIRS.iter().map(PathBuf::from));
                }
                Element::Syslog => config.syslog = true,
                Element::Type(TypeElement { r#type: value }) => config.r#type = Some(value),
                Element::User(s) => config.user = Some(s),
            }
        }

        Ok(config)
    }
}

impl Config {
    pub fn parse(s: &str) -> Result<Self> {
        // TODO: validate that our DOCTYPE and root element are correct
        quick_xml::de::from_str::<Document>(s)?.try_into()
    }

    pub fn read_file(file_path: impl AsRef<Path>) -> Result<Self> {
        // TODO: error message should contain file path to missing `<include>`
        Document::read_file(&file_path)?.try_into()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BusType {
    Session,
    System,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    #[default]
    #[serde(rename = "*")]
    Any,
    MethodCall,
    MethodReturn,
    Signal,
    Error,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Name {
    #[serde(rename = "*")]
    Any,
    Exact(String),
    Prefix(String),
}

const DEFAULT_DATA_DIRS: &[&str] = &["/usr/local/share", "/usr/share"];

const STANDARD_SYSTEM_SERVICEDIRS: &[&str] = &[
    "/usr/local/share/dbus-1/system-services",
    "/usr/share/dbus-1/system-services",
    "/lib/dbus-1/system-services",
];

fn xdg_data_dirs() -> Vec<PathBuf> {
    if let Ok(ok) = var("XDG_DATA_DIRS") {
        return ok.split(":").map(PathBuf::from).collect();
    }
    DEFAULT_DATA_DIRS.iter().map(PathBuf::from).collect()
}

#[cfg(test)]
mod tests;
