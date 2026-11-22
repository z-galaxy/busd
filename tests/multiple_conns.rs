#![cfg(feature = "testing")]

use busd::testing::{init_tracing, with_bus};
use futures_util::future::join_all;
use ntest::timeout;
use tracing::instrument;
use zbus::connection;

#[test]
#[instrument]
#[timeout(30000)]
fn multi_connect() {
    init_tracing();

    busd::testing::block_on(with_bus(|address| async move {
        multi_clients_connect(&address).await
    }));
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
