//! A bus on a socket that listens already, as systemd's socket activation passes one.
#![cfg(target_os = "linux")]

use std::{
    env::temp_dir,
    fs,
    net::TcpListener,
    os::{
        fd::OwnedFd,
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    pin::pin,
};

use busd::bus::Bus;
use futures_util::future::{select, Either};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use zbus::{connection, fdo::DBusProxy};

/// The bus serves its clients on a socket file that it's given, at the address of the socket, and
/// leaves the file to whoever gave it the socket.
#[test]
#[timeout(30000)]
fn socket_file() {
    zruntime::block_on(async {
        let path = temp_dir().join(random_name());
        let listener = UnixListener::bind(&path).unwrap();

        let bus = Bus::for_listener(OwnedFd::from(listener)).await.unwrap();
        let address = bus.address().to_string();
        let expected = format!("unix:path={},guid=", path.display());
        assert!(address.starts_with(&expected), "{address}");
        let bus = serve_a_client(bus).await;

        bus.cleanup().await.unwrap();
        assert!(path.exists(), "the bus removed the file of the socket");
        fs::remove_file(&path).unwrap();
    });
}

/// The bus serves its clients on an abstract socket that it's given.
#[test]
#[timeout(30000)]
fn abstract_socket() {
    zruntime::block_on(async {
        let name = random_name();
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixListener::bind_addr(&addr).unwrap();

        let bus = Bus::for_listener(OwnedFd::from(listener)).await.unwrap();
        let address = bus.address().to_string();
        assert!(
            address.starts_with(&format!("unix:abstract={name},guid=")),
            "{address}"
        );
        serve_a_client(bus).await.cleanup().await.unwrap();
    });
}

/// The bus refuses a TCP socket, as it only listens on unix sockets.
#[test]
#[timeout(30000)]
fn tcp_socket_is_refused() {
    zruntime::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();

        let error = Bus::for_listener(OwnedFd::from(listener))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the bus only listens on unix sockets"),
            "{error}"
        );
    });
}

/// The bus refuses a socket that doesn't listen.
#[test]
#[timeout(30000)]
fn socket_that_does_not_listen() {
    zruntime::block_on(async {
        let (socket, _peer) = UnixStream::pair().unwrap();

        let error = Bus::for_listener(OwnedFd::from(socket)).await.unwrap_err();
        assert!(error.to_string().contains("doesn't listen"), "{error}");
    });
}

/// Run `bus` until a client connects to it and gets its ID, which is that of the bus, and hand the
/// bus back then.
async fn serve_a_client(mut bus: Bus) -> Bus {
    let address = bus.address().to_string();
    let guid = bus.guid().to_string();
    {
        let running = pin!(bus.run());
        let client = pin!(async {
            let conn = connection::Builder::address(address.as_str())
                .build()
                .await
                .unwrap();

            DBusProxy::new(&conn).await.unwrap().get_id().await.unwrap()
        });
        match select(running, client).await {
            Either::Left((result, _)) => panic!("The bus stopped: {result:?}"),
            Either::Right((id, _)) => assert_eq!(id.to_string(), guid),
        }
    }

    bus
}

/// A name that no other socket has.
fn random_name() -> String {
    Alphanumeric.sample_string(&mut rng(), 10)
}
