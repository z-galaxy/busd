use std::{env::temp_dir, pin::pin, time::Duration};

use busd::bus::Bus;
use futures_util::future::{select, Either};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::{connection, fdo::DBusProxy, names::WellKnownName};
use zruntime::{Event, SharedRuntime};

/// Once a bus goes, and its clients with it, nothing is left of it: its peers, monitors and names
/// go too.
#[test]
#[instrument]
#[timeout(30000)]
fn shutdown() {
    busd::tracing_subscriber::init();

    zruntime::block_on(async {
        let s = Alphanumeric.sample_string(&mut rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        let mut bus = Bus::for_address(Some(&address)).await.unwrap();
        let peers = bus.peers().downgrade();
        let stop = Event::new();
        let stopped = stop.listen();

        let handle = zruntime::spawn(async move {
            {
                let running = pin!(bus.run());
                match select(stopped, running).await {
                    Either::Left(_) => (),
                    Either::Right((Ok(()), _)) => panic!("Bus exited unexpectedly"),
                    Either::Right((Err(e), _)) => panic!("Bus exited with an error: {e}"),
                }
            }

            bus
        });

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

        stop.notify(usize::MAX);
        let bus = handle.await.unwrap();
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
