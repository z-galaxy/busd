use anyhow::ensure;
use busd::bus::Bus;
use futures_util::TryStreamExt;
use ntest::timeout;
use tokio::{select, sync::oneshot::Sender};
use tracing::instrument;
use zbus::{
    connection,
    fdo::{DBusProxy, MonitoringProxy, NameAcquired, NameLost, NameOwnerChanged, RequestNameFlags},
    message,
    names::BusName,
    proxy::CacheProperties,
    MessageStream,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[instrument]
#[timeout(15000)]
async fn become_monitor() {
    busd::tracing_subscriber::init();

    let address = "tcp:host=127.0.0.1,port=4242".to_string();
    let mut bus = Bus::for_address(Some(&address)).await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();

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

    let ret = become_monitor_client(&address, tx).await;
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

#[instrument]
async fn become_monitor_client(address: &str, tx: Sender<()>) -> anyhow::Result<()> {
    // Create a monitor that wants all messages.
    let conn = connection::Builder::address(address)?.build().await?;
    let mut msg_stream = MessageStream::from(&conn);
    MonitoringProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .become_monitor(&[], 0)
        .await?;
    let unique_name = BusName::from(conn.unique_name().unwrap().clone());
    drop(conn);

    // Signals for the monitor loosing its unique name.
    let signal = loop {
        let msg = msg_stream.try_next().await?.unwrap();
        // Ignore other messages (e.g `BecomeMonitor` method & reply)
        if let Some(signal) = NameOwnerChanged::from_message(msg) {
            break signal;
        }
    };
    let args = signal.args()?;
    ensure!(
        *args.name() == unique_name,
        "expected NameOwnerChanged signal for monitor's unique_name"
    );
    let signal = loop {
        let msg = msg_stream.try_next().await?.unwrap();
        if let Some(signal) = NameLost::from_message(msg) {
            break signal;
        }
    };
    let args = signal.args()?;
    ensure!(
        *args.name() == unique_name,
        "expected NameLost signal for monitor's unique_name"
    );

    // Now a client that calls a method that triggers a signal.
    let conn = connection::Builder::address(address)?.build().await?;
    let name = "org.z-galaxy.MonitorTest";
    DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .request_name(
            name.try_into()?,
            RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue,
        )
        .await?;

    // Now monitor should have received all messages.
    let mut num_received = 0;
    let mut hello_serial = None;
    let mut request_name_serial = None;
    while num_received < 8 {
        let msg = msg_stream.try_next().await?.unwrap();
        let header = msg.header();
        let member = header.member();

        match msg.message_type() {
            message::Type::MethodCall => match member.unwrap().as_str() {
                "Hello" => {
                    hello_serial = Some(msg.primary_header().serial_num());
                }
                "RequestName" => {
                    request_name_serial = Some(msg.primary_header().serial_num());
                }
                method => panic!("unexpected method call: {method}"),
            },
            message::Type::MethodReturn => {
                let serial = header.reply_serial();
                if serial == hello_serial {
                    hello_serial = None;
                } else if serial == request_name_serial {
                    request_name_serial = None;
                } else {
                    panic!("unexpected method return: {}", serial.unwrap());
                }
            }
            message::Type::Signal => {
                if let Some(signal) = NameOwnerChanged::from_message(msg.clone()) {
                    let args = signal.args()?;
                    ensure!(
                        *args.name() == BusName::from(conn.unique_name().unwrap())
                            || *args.name() == name,
                        "expected NameOwnerChanged signal for one of client's names"
                    );
                } else if let Some(signal) = NameAcquired::from_message(msg) {
                    let args = signal.args()?;
                    ensure!(
                        *args.name() == BusName::from(conn.unique_name().unwrap())
                            || *args.name() == name,
                        "expected NameAcquired signal for one of client's names"
                    );
                }
            }
            _ => panic!("unexpected message type: {:?}", msg.message_type()),
        }

        num_received += 1;
    }

    tx.send(()).unwrap();

    Ok(())
}
