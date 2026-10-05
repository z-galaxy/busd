use std::{path::PathBuf, str::FromStr};

use busd::config::{
    Access, BusType, Config, ConnectOperation, MessageType, Name, NameOwnership, Operation, Policy,
    ReceiveOperation, SendOperation,
};
use zbus::{connection::AuthMechanism, Address};

#[test]
fn config_read_file_with_includes_ok() {
    let got =
        Config::read_file("./tests/data/valid.conf").expect("should read and parse XML input");

    assert_eq!(
        got,
        Config {
            auth: Some(AuthMechanism::External),
            listen: Some(Address::from_str("unix:path=/tmp/a").expect("should parse address")),
            policies: vec![
                Policy::DefaultContext(vec![
                    (
                        Access::Allow,
                        Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    ),
                    (
                        Access::Deny,
                        Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    ),
                ]),
                Policy::MandatoryContext(vec![
                    (
                        Access::Deny,
                        Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    ),
                    (
                        Access::Allow,
                        Operation::Own(NameOwnership {
                            own: Some(Name::Any)
                        })
                    ),
                ],),
            ],
            ..Default::default()
        }
    );
}

#[test]
fn config_read_file_example_session_disable_stats_conf_ok() {
    let got = Config::read_file("./tests/data/example-session-disable-stats.conf")
        .expect("should read and parse XML input");

    assert_eq!(
        got,
        Config {
            policies: vec![Policy::DefaultContext(vec![(
                Access::Deny,
                Operation::Send(SendOperation {
                    broadcast: None,
                    destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                    error: None,
                    interface: Some(String::from("org.freedesktop.DBus.Debug.Stats")),
                    max_fds: None,
                    member: None,
                    min_fds: None,
                    path: None,
                    r#type: None
                }),
            ),]),],
            ..Default::default()
        }
    );
}

#[test]
fn config_read_file_example_system_enable_stats_conf_ok() {
    let got = Config::read_file("./tests/data/example-system-enable-stats.conf")
        .expect("should read and parse XML input");

    assert_eq!(
        got,
        Config {
            policies: vec![Policy::User(
                vec![(
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus.Debug.Stats")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None
                    }),
                )],
                String::from("USERNAME"),
            ),],
            ..Default::default()
        }
    );
}

#[test]
fn config_read_file_session_conf_ok() {
    let mut got =
        Config::read_file("./tests/data/session.conf").expect("should read and parse XML input");

    assert!(!got.servicedirs.is_empty());

    // nuking this to make it easier to `assert_eq!()`
    got.servicedirs = vec![];

    assert_eq!(
        got,
        Config {
            listen: Some(
                Address::from_str("unix:path=/run/user/1000/bus").expect("should parse address")
            ),
            keep_umask: true,
            policies: vec![Policy::DefaultContext(vec![
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Any),
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Own(NameOwnership {
                        own: Some(Name::Any),
                    }),
                ),
            ]),],
            r#type: Some(BusType::Session),
            ..Default::default()
        }
    );
}

#[test]
fn config_read_file_system_conf_ok() {
    let want = Config {
        auth: Some(AuthMechanism::External),
        fork: true,
        listen: Some(
            Address::from_str("unix:path=/var/run/dbus/system_bus_socket")
                .expect("should parse address"),
        ),
        pidfile: Some(PathBuf::from("@DBUS_SYSTEM_PID_FILE@")),
        policies: vec![
            Policy::DefaultContext(vec![
                (
                    Access::Allow,
                    Operation::Connect(ConnectOperation {
                        group: None,
                        user: Some(String::from("*")),
                    }),
                ),
                (
                    Access::Deny,
                    Operation::Own(NameOwnership {
                        own: Some(Name::Any),
                    }),
                ),
                (
                    Access::Deny,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: None,
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: Some(MessageType::MethodCall),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: None,
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: Some(MessageType::Signal),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: None,
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: Some(MessageType::MethodReturn),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: None,
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: Some(MessageType::Error),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Receive(ReceiveOperation {
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        sender: None,
                        r#type: Some(MessageType::MethodCall),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Receive(ReceiveOperation {
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        sender: None,
                        r#type: Some(MessageType::MethodReturn),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Receive(ReceiveOperation {
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        sender: None,
                        r#type: Some(MessageType::Error),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Receive(ReceiveOperation {
                        error: None,
                        interface: None,
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        sender: None,
                        r#type: Some(MessageType::Signal),
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus.Introspectable")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus.Properties")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
                (
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus.Containers1")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
                (
                    Access::Deny,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus")),
                        max_fds: None,
                        member: Some(String::from("UpdateActivationEnvironment")),
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
                (
                    Access::Deny,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus.Debug.Stats")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
                (
                    Access::Deny,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.systemd1.Activator")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                ),
            ]),
            Policy::User(
                vec![(
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.systemd1.Activator")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                )],
                String::from("root"),
            ),
            Policy::User(
                vec![(
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus.Monitoring")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                )],
                String::from("root"),
            ),
            Policy::User(
                vec![(
                    Access::Allow,
                    Operation::Send(SendOperation {
                        broadcast: None,
                        destination: Some(Name::Exact(String::from("org.freedesktop.DBus"))),
                        error: None,
                        interface: Some(String::from("org.freedesktop.DBus.Debug.Stats")),
                        max_fds: None,
                        member: None,
                        min_fds: None,
                        path: None,
                        r#type: None,
                    }),
                )],
                String::from("root"),
            ),
        ],
        servicehelper: Some(PathBuf::from("@DBUS_LIBEXECDIR@/dbus-daemon-launch-helper")),
        syslog: true,
        r#type: Some(BusType::System),
        user: Some(String::from("@DBUS_USER@")),
        ..Default::default()
    };

    let mut got =
        Config::read_file("./tests/data/system.conf").expect("should read and parse XML input");

    assert!(!got.servicedirs.is_empty());

    // nuking this to make it easier to `assert_eq!()`
    got.servicedirs = vec![];

    assert_eq!(got, want,);
}

#[test]
fn config_read_file_real_usr_share_dbus1_session_conf_ok() {
    let config_path = PathBuf::from("/usr/share/dbus-1/session.conf");
    if !config_path.exists() {
        return;
    }
    Config::read_file(config_path).expect("should read and parse XML input");
}

#[test]
fn config_read_file_real_usr_share_dbus1_system_conf_ok() {
    let config_path = PathBuf::from("/usr/share/dbus-1/system.conf");
    if !config_path.exists() {
        return;
    }
    Config::read_file(config_path).expect("should read and parse XML input");
}

#[should_panic]
#[test]
fn config_read_file_with_missing_include_err() {
    Config::read_file("./tests/data/missing_include.conf")
        .expect("should read and parse XML input");
}

#[should_panic]
#[test]
fn config_read_file_with_transitive_missing_include_err() {
    Config::read_file("./tests/data/transitive_missing_include.conf")
        .expect("should read and parse XML input");
}
