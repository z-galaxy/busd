use std::{env::temp_dir, pin::pin};

use anyhow::ensure;
use busd::bus::Bus;
use futures_util::{
    future::{select, Either},
    stream::StreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::{
    connection,
    fdo::{self, DBusProxy, ReleaseNameReply, RequestNameFlags, RequestNameReply},
    names::{BusName, WellKnownName},
    proxy::CacheProperties,
};
use zruntime::Event;

#[test]
#[instrument]
#[timeout(30000)]
fn name_ownership_changes() {
    busd::tracing_subscriber::init();

    zruntime::block_on(async {
        let s = Alphanumeric.sample_string(&mut rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        name_ownership_changes_(&address).await;
    });
}

async fn name_ownership_changes_(address: &str) {
    let mut bus = Bus::for_address(Some(address)).await.unwrap();
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

    let ret = name_ownership_changes_client(address).await;
    stop.notify(usize::MAX);
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

#[instrument]
async fn name_ownership_changes_client(address: &str) -> anyhow::Result<()> {
    let conn = connection::Builder::address(address).build().await?;
    let conn_unique_name = conn.unique_name().unwrap().to_owned();
    let dbus_proxy = DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let name: WellKnownName = "org.blah".try_into()?;

    let mut name_changed_stream = dbus_proxy.receive_name_owner_changed().await?;
    let mut name_acquired_stream = dbus_proxy.receive_name_acquired().await?;
    // This should work.
    let ret = dbus_proxy
        .request_name(name.clone(), RequestNameFlags::ALLOW_REPLACEMENT)
        .await?;
    ensure!(
        ret == RequestNameReply::PrimaryOwner,
        "expected to become primary owner"
    );
    // Ensure signals were emitted.
    let mut changed = name_changed_stream.next().await.unwrap();
    if *changed.args()?.name() == *conn_unique_name {
        // In case we do happen to get the signal for our unique name, well-known name signal should
        // be next.
        changed = name_changed_stream.next().await.unwrap();
    }
    ensure!(
        *changed.args()?.name() == name,
        "expected name owner changed signal for our name"
    );
    ensure!(
        changed.args()?.old_owner.is_none(),
        "expected no old owner for our name"
    );
    ensure!(
        changed.args()?.new_owner.as_ref().unwrap() == conn.unique_name().unwrap(),
        "expected new owner to be us"
    );
    ensure!(
        changed.message().header().destination().is_none(),
        "expected no destination for our signal",
    );
    let acquired = name_acquired_stream.next().await.unwrap();
    ensure!(
        *acquired.args()?.name() == name,
        "expected name acquired signal for our name"
    );
    ensure!(
        *acquired.message().header().destination().unwrap()
            == BusName::from(conn.unique_name().unwrap()),
        "expected name acquired signal to be unicasted to the acquiring connection",
    );

    // This shouldn't and we should be told we already own the name.
    let ret = dbus_proxy
        .request_name(name.clone(), RequestNameFlags::ALLOW_REPLACEMENT)
        .await?;
    ensure!(
        ret == RequestNameReply::AlreadyOwner,
        "expected to be already primary owner"
    );

    // Now we try with another connection and we should be queued.
    let conn2 = connection::Builder::address(address).build().await?;
    let conn2_unique_name = conn2.unique_name().unwrap().to_owned();
    let changed = name_changed_stream.next().await.unwrap();
    ensure!(
        *changed.args()?.name() == *conn2_unique_name,
        "expected name owner changed signal for the new connections gaining unique name"
    );
    ensure!(
        changed.args()?.old_owner.is_none(),
        "expected no old owner for the unique name of the second connection"
    );
    ensure!(
        *changed.args()?.new_owner.as_ref().unwrap() == conn2_unique_name,
        "expected new owner of the unique name of the second connection to be itself"
    );
    let dbus_proxy2 = DBusProxy::builder(&conn2)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let ret = dbus_proxy2
        .request_name(name.clone(), RequestNameFlags::empty())
        .await?;

    // Check that first client is the primary owner before it releases the name.
    ensure!(ret == RequestNameReply::InQueue, "expected to be in queue");
    let owner = dbus_proxy.get_name_owner(name.clone().into()).await?;
    let unique_name = conn.unique_name().unwrap().clone();
    ensure!(owner == unique_name, "unexpected owner");
    let owner = dbus_proxy
        .get_name_owner(unique_name.clone().into())
        .await?;
    ensure!(owner == unique_name, "unexpected owner");
    let res = dbus_proxy.get_name_owner(":1.3333".try_into()?).await;
    ensure!(
        matches!(res, Err(fdo::Error::NameHasNoOwner(_))),
        "expected error"
    );

    let mut name_acquired_stream = dbus_proxy2.receive_name_acquired().await?;
    let mut name_lost_stream = dbus_proxy.receive_name_lost().await?;
    // Now the first client releases name.
    let ret = dbus_proxy.release_name(name.clone()).await?;
    ensure!(
        ret == ReleaseNameReply::Released,
        "expected name to be released"
    );
    // Ensure signals were emitted.
    let changed = name_changed_stream.next().await.unwrap();
    ensure!(
        *changed.args()?.name() == name,
        "expected name owner changed signal for our name"
    );
    ensure!(
        changed.args()?.old_owner.as_ref().unwrap() == conn.unique_name().unwrap(),
        "expected old owner to be our first connection"
    );
    ensure!(
        changed.args()?.new_owner.as_ref().unwrap() == conn2.unique_name().unwrap(),
        "expected new owner to be our second connection"
    );
    ensure!(
        changed.message().header().destination().is_none(),
        "expected no destination for our signal",
    );
    let lost = name_lost_stream.next().await.unwrap();
    ensure!(
        *lost.args()?.name() == name,
        "expected name lost signal for our name"
    );
    ensure!(
        *lost.message().header().destination().unwrap()
            == BusName::from(conn.unique_name().unwrap()),
        "expected name lost signal to be unicasted to the loosing connection",
    );
    let acquired = name_acquired_stream.next().await.unwrap();
    ensure!(
        *acquired.args()?.name() == name,
        "expected name acquired signal for our name"
    );
    ensure!(
        *acquired.message().header().destination().unwrap()
            == BusName::from(conn2.unique_name().unwrap()),
        "expected name acquired signal to be unicasted to the acquiring connection",
    );

    // Now the second client should be the primary owner.
    let owner = dbus_proxy.get_name_owner(name.clone().into()).await?;
    ensure!(owner == *conn2.unique_name().unwrap(), "unexpected owner");

    drop(name_acquired_stream);
    drop(dbus_proxy2);
    drop(conn2);

    let mut unique_name_signaled = false;
    let mut well_known_name_signaled = false;
    while !unique_name_signaled || !well_known_name_signaled {
        let changed = name_changed_stream.next().await.unwrap();
        if *changed.args()?.name() == *conn2_unique_name {
            ensure!(
                changed.args()?.new_owner.is_none(),
                "expected no new owner for our unique name"
            );
            ensure!(
                *changed.args()?.old_owner.as_ref().unwrap() == conn2_unique_name,
                "expected old owner to be us"
            );
            unique_name_signaled = true;
        } else if *changed.args()?.name() == name {
            ensure!(
                changed.args()?.new_owner.is_none(),
                "expected no new owner for our name"
            );
            ensure!(
                *changed.args()?.old_owner.as_ref().unwrap() == conn2_unique_name,
                "expected old owner to be us"
            );
            well_known_name_signaled = true;
        } else {
            panic!("unexpected name owner changed signal");
        }
    }

    Ok(())
}
