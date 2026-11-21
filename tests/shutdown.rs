#![cfg(feature = "testing")]

use std::time::Duration;

use busd::{
    bus::Bus,
    testing::{init_tracing, unix_address, Running},
};
use ntest::timeout;
use tracing::instrument;
use zbus::{connection, fdo::DBusProxy, names::WellKnownName};
use zruntime::SharedRuntime;

/// Once a bus goes, and its clients with it, nothing is left of it: its peers, monitors and names
/// go too.
#[test]
#[instrument]
#[timeout(30000)]
fn shutdown() {
    init_tracing();

    zruntime::block_on(async {
        let address = unix_address();
        let bus = Bus::for_address(Some(&address)).await.unwrap();
        let peers = bus.peers().downgrade();
        let running = Running::new(bus);

        // A client that owns a name, and goes, which has the bus signal changes of owners.
        let client = connection::Builder::address(address.as_str())
            .build()
            .await
            .unwrap();
        let name = WellKnownName::try_from("org.zbus.Shutdown").unwrap();
        DBusProxy::new(&client)
            .await
            .unwrap()
            .request_name(name, Default::default())
            .await
            .unwrap();
        drop(client);

        let bus = running.stop().await;
        bus.cleanup().await.unwrap();

        // The tasks that served the peers end in their own time.
        let runtime = SharedRuntime::current().unwrap();
        runtime
            .timeout(Duration::from_secs(10), async {
                while peers.upgrade().is_some() {
                    runtime.sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the peers outlived the bus");
    });
}
