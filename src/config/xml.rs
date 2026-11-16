use std::{
    env::current_dir,
    ffi::OsString,
    fs::{read_dir, read_to_string},
    io,
    path::{Path, PathBuf},
    str::FromStr,
};

use crate::{debug, error, warn};
use anyhow::{Error, Result};
use serde::Deserialize;

use super::{
    rule::{
        Access, ConnectOperation, NameOwnership, Operation, ReceiveOperation, Rule, SendOperation,
    },
    BusType, MessageType, Name, Policy,
};

/// The bus configuration.
///
/// This is currently only loaded from the [XML configuration files] defined by the specification.
/// We plan to add support for other formats (e.g JSON) in the future.
///
/// [XML configuration files]: https://dbus.freedesktop.org/doc/dbus-daemon.1.html#configuration_file
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Document {
    #[serde(rename = "$value", default)]
    pub busconfig: Vec<Element>,
    file_path: Option<PathBuf>,
}

impl FromStr for Document {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        quick_xml::de::from_str(s).map_err(Error::msg)
    }
}

impl Document {
    pub fn read_file(file_path: impl AsRef<Path>) -> Result<Document> {
        Document::read_included(file_path.as_ref(), &mut Vec::new())
    }

    /// The document of the file at `file_path`, which the files of `ancestors` led to: each of them
    /// includes the next, and the last one includes this file. They go by their canonical paths.
    fn read_included(file_path: &Path, ancestors: &mut Vec<PathBuf>) -> Result<Document> {
        let text = read_to_string(file_path)?;

        let mut doc = Document::from_str(&text)?;
        doc.file_path = Some(file_path.to_path_buf());
        let doc = doc.resolve_includedirs()?;
        let Ok(canonical) = file_path.canonicalize() else {
            return doc.resolve_includes(ancestors);
        };
        // The file is one of the ancestors of the files that it includes.
        ancestors.push(canonical);
        let doc = doc.resolve_includes(ancestors);
        ancestors.pop();

        doc
    }

    fn resolve_includedirs(self) -> Result<Document> {
        let base_path = self.base_path()?;
        let Document {
            busconfig,
            file_path,
        } = self;

        let mut doc = Document {
            busconfig: vec![],
            file_path: None,
        };

        for el in busconfig {
            match el {
                Element::Includedir(dir_path) => {
                    let dir_path = resolve_include_path(&base_path, &dir_path);
                    let dir_path = match dir_path.canonicalize() {
                        Ok(ok) => ok,
                        // we treat `<includedir>` as though it has `ignore_missing="yes"`
                        Err(err) => {
                            let message = format!(
                                "cannot resolve '<includedir>{}</includedir>' to an absolute path: {}",
                                dir_path.display(),
                                err
                            );
                            // The configuration of the reference implementation names
                            // directories that many systems lack.
                            if err.kind() == io::ErrorKind::NotFound {
                                debug!("{message}");
                            } else {
                                warn!("{message}");
                            }
                            continue;
                        }
                    };
                    match read_dir(&dir_path) {
                        Ok(ok) => {
                            for entry in ok {
                                let path = entry?.path();
                                if path.extension() == Some(&OsString::from("conf"))
                                    && path.is_file()
                                {
                                    doc.busconfig.push(Element::Include(IncludeElement {
                                        file_path: path,
                                        ..Default::default()
                                    }));
                                }
                            }
                        }
                        // we treat `<includedir>` as though it has `ignore_missing="yes"`
                        Err(err) => {
                            warn!(
                                "cannot read '<includedir>{}</includedir>': {}",
                                &dir_path.display(),
                                err
                            );
                            continue;
                        }
                    }
                }
                _ => doc.busconfig.push(el),
            }
        }

        doc.file_path = file_path;
        Ok(doc)
    }

    /// The document, with the elements of each file that it includes in place of the `<include>`.
    ///
    /// `ancestors` are the files that are being read on the way to this document, its own included.
    /// An `<include>` of one of them is ignored, with a warning, as dbus-broker does, since reading
    /// the file again would never end. A file that several files include is no such thing, and is
    /// read each time.
    fn resolve_includes(self, ancestors: &mut Vec<PathBuf>) -> Result<Document> {
        let base_path = self.base_path()?;
        let Document {
            busconfig,
            file_path,
        } = self;

        let mut doc = Document {
            busconfig: vec![],
            file_path: None,
        };

        for el in busconfig {
            match el {
                Element::Include(include) => {
                    if include.if_selinux_enable == IncludeOption::Yes
                        || include.selinux_root_relative == IncludeOption::Yes
                    {
                        // TODO: implement SELinux support
                        continue;
                    }

                    let ignore_missing = include.ignore_missing == IncludeOption::Yes;
                    let file_path = resolve_include_path(&base_path, &include.file_path);
                    let file_path = match file_path.canonicalize() {
                        Ok(ok) => ok,
                        Err(err) => {
                            let message = format!(
                                "cannot resolve '<include>{}</include>' to an absolute path: {}",
                                file_path.display(),
                                err
                            );
                            // The configuration of the reference implementation names files
                            // that it does without, and that many systems lack.
                            if ignore_missing && err.kind() == io::ErrorKind::NotFound {
                                debug!("{message}");
                                continue;
                            }
                            if ignore_missing {
                                warn!("{message}");
                                continue;
                            }
                            error!("{message}");
                            return Err(Error::msg(err));
                        }
                    };
                    if ancestors.contains(&file_path) {
                        warn!(
                            "ignoring '<include>{}</include>': the file is being read already, \
                             so including it again would never end",
                            file_path.display()
                        );
                        continue;
                    }
                    let mut included = match Document::read_included(&file_path, ancestors) {
                        Ok(ok) => ok,
                        Err(err) => {
                            if ignore_missing {
                                warn!("'{}' should contain valid XML", include.file_path.display());
                                continue;
                            }
                            error!("'{}' should contain valid XML", include.file_path.display());
                            return Err(err);
                        }
                    };
                    doc.busconfig.append(&mut included.busconfig);
                }
                _ => doc.busconfig.push(el),
            }
        }

        doc.file_path = file_path;
        Ok(doc)
    }

    fn base_path(&self) -> Result<PathBuf> {
        match &self.file_path {
            Some(some) => Ok(some
                .parent()
                .ok_or_else(|| Error::msg("`<include>` path should contain a file name"))?
                .to_path_buf()),
            None => {
                warn!("cannot determine file path for this XML document, using current working directory");
                current_dir().map_err(Error::msg)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Element {
    AllowAnonymous,
    Auth,
    Fork,
    /// Include a file at this point. If the filename is relative, it is located relative to the
    /// configuration file doing the including.
    Include(IncludeElement),
    /// Files in the directory are included in undefined order.
    /// Only files ending in ".conf" are included.
    Includedir(PathBuf),
    KeepUmask,
    Listen(String),
    Limit,
    Pidfile(PathBuf),
    Policy(PolicyElement),
    Servicedir(PathBuf),
    Servicehelper(PathBuf),
    /// Requests a standard set of session service directories.
    /// Its effect is similar to specifying a series of `<servicedir/>` elements for each of the
    /// data directories, in the order given here.
    StandardSessionServicedirs,
    /// Specifies the standard system-wide activation directories that should be searched for
    /// service files.
    StandardSystemServicedirs,
    Syslog,
    Type(TypeElement),
    User(String),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct IncludeElement {
    #[serde(default, rename = "@ignore_missing")]
    ignore_missing: IncludeOption,

    // TODO: implement SELinux
    #[serde(default, rename = "@if_selinux_enabled")]
    if_selinux_enable: IncludeOption,
    #[serde(default, rename = "@selinux_root_relative")]
    selinux_root_relative: IncludeOption,

    #[serde(rename = "$value")]
    file_path: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IncludeOption {
    #[default]
    No,
    Yes,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyContext {
    Default,
    Mandatory,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct PolicyElement {
    #[serde(rename = "@at_console")]
    pub at_console: Option<String>,
    #[serde(rename = "@context")]
    pub context: Option<PolicyContext>,
    #[serde(rename = "@group")]
    pub group: Option<String>,
    #[serde(rename = "$value", default)]
    pub rules: Vec<RuleElement>,
    #[serde(rename = "@user")]
    pub user: Option<String>,
}

impl PolicyElement {
    /// The policy that the element represents, or `None` if the policy is left out.
    pub(super) fn into_policy(self) -> Result<Option<Policy>> {
        match self {
            // Ignore the rules of the unsupported `at_console` context, but keep a placeholder for
            // the policy. Otherwise, a configuration of a bus other than a system bus that has
            // only such policies would fall back to the open policy.
            PolicyElement {
                at_console: Some(_),
                context: None,
                group: None,
                user: None,
                ..
            } => {
                warn!(
                    "Ignoring the rules of an `at_console` policy, which the bus doesn't support"
                );

                Ok(Some(Policy::Unsupported))
            }
            PolicyElement {
                at_console: None,
                context: Some(c),
                group: None,
                rules,
                user: None,
            } => Ok(Some(match c {
                PolicyContext::Default => Policy::DefaultContext(rules_of(rules)?),
                PolicyContext::Mandatory => Policy::MandatoryContext(rules_of(rules)?),
            })),
            PolicyElement {
                at_console: None,
                context: None,
                group: Some(group),
                rules,
                user: None,
            } => Ok(Some(Policy::Group(rules_of(rules)?, group))),
            PolicyElement {
                at_console: None,
                context: None,
                group: None,
                rules,
                user: Some(user),
            } => Ok(Some(Policy::User(rules_of(rules)?, user))),
            _ => Err(Error::msg(format!(
                "policy contains conflicting attributes: {self:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct RuleAttributes {
    #[serde(rename = "@max_fds")]
    pub max_fds: Option<u32>,
    #[serde(rename = "@min_fds")]
    pub min_fds: Option<u32>,

    #[serde(rename = "@receive_error")]
    pub receive_error: Option<String>,
    #[serde(rename = "@receive_interface")]
    pub receive_interface: Option<String>,
    /// deprecated and ignored
    #[serde(rename = "@receive_member")]
    pub receive_member: Option<String>,
    #[serde(rename = "@receive_path")]
    pub receive_path: Option<String>,
    #[serde(rename = "@receive_sender")]
    pub receive_sender: Option<String>,
    #[serde(rename = "@receive_type")]
    pub receive_type: Option<MessageType>,

    #[serde(rename = "@send_broadcast")]
    pub send_broadcast: Option<bool>,
    #[serde(rename = "@send_destination")]
    pub send_destination: Option<String>,
    #[serde(rename = "@send_destination_prefix")]
    pub send_destination_prefix: Option<String>,
    #[serde(rename = "@send_error")]
    pub send_error: Option<String>,
    #[serde(rename = "@send_interface")]
    pub send_interface: Option<String>,
    #[serde(rename = "@send_member")]
    pub send_member: Option<String>,
    #[serde(rename = "@send_path")]
    pub send_path: Option<String>,
    #[serde(rename = "@send_type")]
    pub send_type: Option<MessageType>,

    /// deprecated and ignored
    #[serde(rename = "@receive_requested_reply")]
    pub receive_requested_reply: Option<bool>,
    /// deprecated and ignored
    #[serde(rename = "@send_requested_reply")]
    pub send_requested_reply: Option<bool>,

    /// deprecated and ignored
    #[serde(rename = "@eavesdrop")]
    pub eavesdrop: Option<bool>,

    #[serde(rename = "@own")]
    pub own: Option<String>,
    #[serde(rename = "@own_prefix")]
    pub own_prefix: Option<String>,

    #[serde(rename = "@group")]
    pub group: Option<String>,
    #[serde(rename = "@user")]
    pub user: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RuleElement {
    Allow(RuleAttributes),
    Deny(RuleAttributes),
}

impl RuleElement {
    /// The rule that the element represents, or `None` if the rule is left out.
    pub(super) fn into_rule(self) -> Result<Option<Rule>> {
        match self {
            RuleElement::Allow(RuleAttributes {
                group: Some(_),
                user: Some(_),
                ..
            })
            | RuleElement::Deny(RuleAttributes {
                group: Some(_),
                user: Some(_),
                ..
            }) => Err(Error::msg(format!(
                "`group` cannot be combined with `user` in the same rule: {self:?}"
            ))),
            RuleElement::Allow(RuleAttributes {
                own: Some(_),
                own_prefix: Some(_),
                ..
            })
            | RuleElement::Deny(RuleAttributes {
                own: Some(_),
                own_prefix: Some(_),
                ..
            }) => Err(Error::msg(format!(
                "`own_prefix` cannot be combined with `own` in the same rule: {self:?}"
            ))),
            RuleElement::Allow(RuleAttributes {
                send_destination: Some(_),
                send_destination_prefix: Some(_),
                ..
            })
            | RuleElement::Deny(RuleAttributes {
                send_destination: Some(_),
                send_destination_prefix: Some(_),
                ..
            }) => Err(Error::msg(format!(
                "`send_destination_prefix` cannot be combined with `send_destination` in the same rule: {self:?}"
            ))),
            RuleElement::Allow(attrs) => Ok(attrs.into_operation()?.map(|operation| Rule {
                access: Access::Allow,
                operation,
            })),
            RuleElement::Deny(RuleAttributes {
                eavesdrop: Some(true),
                ..
            }) => {
                // Such a rule only concerns eavesdropping, which the bus does not do.
                Ok(None)
            }
            RuleElement::Deny(attrs) => Ok(attrs.into_operation()?.map(|operation| Rule {
                access: Access::Deny,
                operation,
            })),
        }
    }
}

impl RuleAttributes {
    /// The operation that the attributes represent, or `None` if the rule is left out.
    fn into_operation(self) -> Result<Option<Operation>> {
        let has_connect = self.group.is_some() || self.user.is_some();
        let has_own = self.own.is_some() || self.own_prefix.is_some();
        // The `*_requested_reply` attributes make a send or receive rule, but are ignored
        // otherwise: they only concern replies, which the policy of the bus does not apply to.
        let has_send = self.send_broadcast.is_some()
            || self.send_destination.is_some()
            || self.send_destination_prefix.is_some()
            || self.send_error.is_some()
            || self.send_interface.is_some()
            || self.send_member.is_some()
            || self.send_path.is_some()
            || self.send_requested_reply.is_some()
            || self.send_type.is_some();
        let has_receive = self.receive_error.is_some()
            || self.receive_interface.is_some()
            || self.receive_member.is_some()
            || self.receive_path.is_some()
            || self.receive_sender.is_some()
            || self.receive_requested_reply.is_some()
            || self.receive_type.is_some();

        let operations_count: i8 = vec![has_connect, has_own, has_receive, has_send]
            .into_iter()
            .map(i8::from)
            .sum();

        if operations_count > 1 {
            return Err(Error::msg(format!("do not mix rule attributes for connect, own, receive, and/or send attributes in the same rule: {self:?}")));
        }

        if has_connect {
            Ok(Some(Operation::Connect(self.into_connect())))
        } else if has_own {
            Ok(Some(Operation::Own(self.into_own())))
        } else if has_receive {
            Ok(Some(Operation::Receive(self.into_receive())))
        } else if has_send {
            Ok(Some(Operation::Send(self.into_send())))
        } else if self.eavesdrop.is_some() {
            // `eavesdrop` is ignored, so a rule that has nothing else is a receive rule for every
            // message: `<allow eavesdrop="true"/>` is how the stock session.conf lets everyone
            // receive everything.
            Ok(Some(Operation::Receive(self.into_receive())))
        } else {
            Err(Error::msg(format!("rule must specify supported attributes for connect, own, receive, or send operations: {self:?}")))
        }
    }

    fn into_connect(self) -> ConnectOperation {
        ConnectOperation {
            group: self.group,
            user: self.user,
        }
    }

    fn into_own(self) -> NameOwnership {
        let own = match self {
            RuleAttributes {
                own: Some(some),
                own_prefix: None,
                ..
            } if some == "*" => Some(Name::Any),
            RuleAttributes {
                own: Some(some),
                own_prefix: None,
                ..
            } => Some(Name::Exact(some)),
            RuleAttributes {
                own: None,
                own_prefix: Some(some),
                ..
            } => Some(Name::Prefix(some)),
            _ => None,
        };
        NameOwnership { own }
    }

    fn into_receive(self) -> ReceiveOperation {
        ReceiveOperation {
            error: self.receive_error,
            interface: self.receive_interface,
            max_fds: self.max_fds,
            member: self.receive_member,
            min_fds: self.min_fds,
            path: self.receive_path,
            sender: self.receive_sender,
            r#type: self.receive_type,
        }
    }

    fn into_send(self) -> SendOperation {
        let destination = match self {
            RuleAttributes {
                send_destination: Some(some),
                send_destination_prefix: None,
                ..
            } if some == "*" => Some(Name::Any),
            RuleAttributes {
                send_destination: Some(some),
                send_destination_prefix: None,
                ..
            } => Some(Name::Exact(some)),
            RuleAttributes {
                send_destination: None,
                send_destination_prefix: Some(some),
                ..
            } => Some(Name::Prefix(some)),
            _ => None,
        };
        SendOperation {
            broadcast: self.send_broadcast,
            destination,
            error: self.send_error,
            interface: self.send_interface,
            max_fds: self.max_fds,
            member: self.send_member,
            min_fds: self.min_fds,
            path: self.send_path,
            r#type: self.send_type,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TypeElement {
    #[serde(rename = "$text")]
    pub r#type: BusType,
}

/// The rules of `elements`, without those that are left out.
fn rules_of(elements: Vec<RuleElement>) -> Result<Vec<Rule>> {
    elements
        .into_iter()
        .filter_map(|element| element.into_rule().transpose())
        .collect()
}

fn resolve_include_path(base_path: impl AsRef<Path>, include_path: impl AsRef<Path>) -> PathBuf {
    let p = include_path.as_ref();
    if p.is_absolute() {
        return p.to_path_buf();
    }

    base_path.as_ref().join(p)
}
