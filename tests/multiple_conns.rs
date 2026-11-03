use std::{env::temp_dir, pin::pin};

use busd::bus::Bus;
use futures_util::future::{join_all, select, Either};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::connection;
use zruntime::Event;

#[test]
#[instrument]
#[timeout(30000)]
fn multi_conenct() {
    busd::tracing_subscriber::init();

    zruntime::block_on(async {
        let s = Alphanumeric.sample_string(&mut rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        multi_conenct_(&address).await;
    });
}

async fn multi_conenct_(socket_addr: &str) {
    let mut bus = Bus::for_address(Some(socket_addr)).await.unwrap();
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

    let ret = multi_clients_connect(socket_addr).await;
    stop.notify(usize::MAX);
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

#[instrument]
async fn multi_clients_connect(socket_addr: &str) -> anyhow::Result<()> {
    // Create 10 connections simultaneously.
    let conns: Vec<_> = (0..10)
        .map(|_| connection::Builder::address(socket_addr).build())
        .collect();
    join_all(conns).await;

    Ok(())
}
