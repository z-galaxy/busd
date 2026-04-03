use std::env::temp_dir;

use busd::bus::Bus;
use futures_util::future::join_all;
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tokio::{select, sync::oneshot::channel};
use tracing::instrument;
use zbus::connection;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[instrument]
#[timeout(30000)]
async fn multi_conenct() {
    busd::tracing_subscriber::init();

    // Unix socket
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    multi_conenct_(&address).await;

    // TCP socket
    let address = "tcp:host=127.0.0.1,port=4246".to_string();
    multi_conenct_(&address).await;
}

async fn multi_conenct_(socket_addr: &str) {
    let mut bus = Bus::for_address(Some(socket_addr)).await.unwrap();
    let (tx, rx) = channel();

    let handle = tokio::spawn(async move {
        select! {
            _ = rx => (),
            res = bus.run() => match res {
                Ok(()) => panic!("Bus exited unexpectedly"),
                Err(e) => panic!("Bus exited with an error: {e}"),
            }
        }

        bus
    });

    let ret = multi_clients_connect(socket_addr).await;
    let _ = tx.send(());
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

#[instrument]
async fn multi_clients_connect(socket_addr: &str) -> anyhow::Result<()> {
    // Create 10 connections simultaneously.
    let conns: Vec<_> = (0..10)
        .map(|_| connection::Builder::address(socket_addr).unwrap().build())
        .collect();
    join_all(conns).await;

    Ok(())
}
