use std::{
    env::current_dir,
    ffi::OsString,
    fs::{read_dir, read_to_string},
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Error, Result};
use serde::Deserialize;
use tracing::{error, warn};

use super::{BusType, MessageType};

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
        let text = read_to_string(file_path.as_ref())?;

        let mut doc = Document::from_str(&text)?;
        doc.file_path = Some(file_path.as_ref().to_path_buf());
        doc.resolve_includedirs()?.resolve_includes()
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
                            warn!(
                                "cannot resolve '<includedir>{}</includedir>' to an absolute path: {}",
                                &dir_path.display(),
                                err
                            );
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

    fn resolve_includes(self) -> Result<Document> {
        // TODO: implement protection against circular `<include>` references
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
                    let file_path = match file_path.canonicalize().map_err(Error::msg) {
                        Ok(ok) => ok,
                        Err(err) => {
                            let msg = format!(
                                "cannot resolve '<include>{}</include>' to an absolute path: {}",
                                file_path.display(),
                                err
                            );
                            if ignore_missing {
                                warn!(msg);
                                continue;
                            }
                            error!(msg);
                            return Err(err);
                        }
                    };
                    let mut included = match Document::read_file(&file_path) {
                        Ok(ok) => ok,
                        Err(err) => {
                            let msg = format!(
                                "'{}' should contain valid XML",
                                include.file_path.display()
                            );
                            if ignore_missing {
                                warn!(msg);
                                continue;
                            }
                            error!(msg);
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
    Auth(String),
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
    /// Its effect is similar to specifying a series of <servicedir/> elements for each of the data
    /// directories, in the order given here.
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

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TypeElement {
    #[serde(rename = "$text")]
    pub r#type: BusType,
}

fn resolve_include_path(base_path: impl AsRef<Path>, include_path: impl AsRef<Path>) -> PathBuf {
    let p = include_path.as_ref();
    if p.is_absolute() {
        return p.to_path_buf();
    }

    base_path.as_ref().join(p)
}
