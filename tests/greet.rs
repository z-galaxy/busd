use std::{env::temp_dir, time::Duration};

use anyhow::anyhow;
use busd::bus::Bus;
use futures_util::{pin_mut, stream::StreamExt};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tokio::{select, sync::mpsc::channel, time::timeout};
use tracing::instrument;
use zbus::{
    connection,
    fdo::{self, DBusProxy},
    interface, message,
    object_server::SignalEmitter,
    proxy,
    proxy::CacheProperties,
    zvariant::ObjectPath,
    AsyncDrop, Connection, MatchRule, MessageStream,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[instrument]
#[timeout(30000)]
async fn greet() {
    busd::tracing_subscriber::init();

    // Unix socket
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    greet_(&address).await;

    // TCP socket
    let address = "tcp:host=127.0.0.1,port=4248".to_string();
    greet_(&address).await;
}

async fn greet_(socket_addr: &str) {
    let mut bus = Bus::for_address(Some(socket_addr)).await.unwrap();
    let (tx, mut rx) = channel(1);

    let handle = tokio::spawn(async move {
        select! {
            _ = rx.recv() => (),
            res = bus.run() => match res {
                Ok(()) => panic!("Bus exited unexpectedly"),
                Err(e) => panic!("Bus exited with an error: {e}"),
            }
        }

        bus
    });

    let ret = match greet_service(socket_addr).await {
        Ok(service_conn) => greet_client(socket_addr).await.map(|_| service_conn),
        Err(e) => Err(e),
    };
    let _ = tx.send(()).await;
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    let _ = ret.unwrap();
}

#[instrument]
async fn greet_service(socket_addr: &str) -> anyhow::Result<Connection> {
    struct Greeter {
        count: u64,
    }

    #[interface(name = "org.zbus.MyGreeter1")]
    impl Greeter {
        async fn say_hello(
            &mut self,
            name: &str,
            unicast: bool,
            #[zbus(signal_emitter)] ctxt: SignalEmitter<'_>,
            #[zbus(header)] header: message::Header<'_>,
        ) -> fdo::Result<String> {
            self.count += 1;
            let path = header.path().unwrap().clone();
            let ctxt = if unicast {
                ctxt.set_destination(zbus::names::BusName::Unique(
                    header.sender().unwrap().to_owned(),
                ))
            } else {
                ctxt
            };
            Self::greeted(&ctxt, name, self.count, path).await?;
            Ok(format!(
                "Hello {}! I have been called {} times.",
                name, self.count
            ))
        }

        #[zbus(signal)]
        async fn greeted(
            ctxt: &SignalEmitter<'_>,
            name: &str,
            count: u64,
            path: ObjectPath<'_>,
        ) -> zbus::Result<()>;
    }

    let greeter = Greeter { count: 0 };
    connection::Builder::address(socket_addr)?
        .name("org.zbus.MyGreeter")?
        .serve_at("/org/zbus/MyGreeter", greeter)?
        .build()
        .await
        .map_err(Into::into)
}

#[instrument]
async fn greet_client(socket_addr: &str) -> anyhow::Result<()> {
    #[proxy(
        interface = "org.zbus.MyGreeter1",
        default_path = "/org/zbus/MyGreeter"
    )]
    trait MyGreeter {
        fn say_hello(&self, name: &str, unicast: bool) -> zbus::Result<String>;

        #[zbus(signal)]
        async fn greeted(name: &str, count: u64, path: ObjectPath<'_>);
    }

    let conn = connection::Builder::address(socket_addr)?.build().await?;

    let proxy = MyGreeterProxy::builder(&conn)
        .destination("org.zbus.MyGreeter")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let mut greeted_stream = proxy.receive_greeted().await?;
    let reply = proxy.say_hello("Maria", false).await?;
    assert_eq!(reply, "Hello Maria! I have been called 1 times.");
    let signal = greeted_stream
        .next()
        .await
        .ok_or(anyhow!("stream ended unexpectedly"))?;
    let args = signal.args()?;
    assert_eq!(args.name, "Maria");
    assert_eq!(args.count, 1);
    assert_eq!(args.path, "/org/zbus/MyGreeter");

    // Now let's unsubcribe from the signal and ensure we don't receive it anymore.
    greeted_stream.async_drop().await;
    let msg_stream = MessageStream::from(&conn).filter_map(|msg| async {
        let msg = msg.ok()?;
        Greeted::from_message(msg)
    });
    pin_mut!(msg_stream);
    let _ = proxy.say_hello("Maria", false).await?;
    timeout(Duration::from_millis(10), msg_stream.next())
        .await
        .unwrap_err();

    // ..unless it's targeted to us specifically
    let _ = proxy.say_hello("Maria", true).await?;
    let signal = timeout(Duration::from_millis(10), msg_stream.next())
        .await?
        .unwrap();
    let args = signal.args()?;
    assert_eq!(args.name, "Maria");

    // Now let's try a manual subscription.
    let match_rule = MatchRule::builder()
        .interface("org.zbus.MyGreeter1")?
        .member("Greeted")?
        .add_arg("Maria")?
        .arg_path(2, "/org/zbus/MyGreeter")?
        .build();
    DBusProxy::new(&conn)
        .await?
        .add_match_rule(match_rule)
        .await?;
    let _ = proxy.say_hello("Maria", false).await?;
    let signal = msg_stream.next().await.unwrap();
    let args = signal.args()?;
    assert_eq!(args.name, "Maria");

    Ok(())
}
