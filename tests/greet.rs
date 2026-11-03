use std::{env::temp_dir, pin::pin, time::Duration};

use anyhow::anyhow;
use busd::bus::Bus;
use futures_util::{
    future::{select, Either},
    pin_mut,
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
    fdo::{self, DBusProxy},
    interface, message,
    object_server::SignalEmitter,
    proxy,
    proxy::CacheProperties,
    Address, AsyncDrop, Connection, MatchRule, MessageStream, ObjectPath,
};
use zruntime::{Event, SharedRuntime};

#[test]
#[instrument]
#[timeout(30000)]
fn greet() {
    busd::tracing_subscriber::init();

    zruntime::block_on(async {
        // Unix socket
        let s = Alphanumeric.sample_string(&mut rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        greet_(&address).await;

        // Abstract Unix socket, which only Linux has.
        #[cfg(target_os = "linux")]
        {
            let s = Alphanumeric.sample_string(&mut rng(), 10);
            let address = format!("unix:abstract={s}");
            greet_(&address).await;
        }
    });
}

/// The bus refuses to listen on anything but a unix socket.
#[test]
#[timeout(30000)]
fn tcp_refused() {
    zruntime::block_on(async {
        let address = "tcp:host=127.0.0.1,port=4248";
        let error = Bus::for_address(Some(address)).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Unsupported address `{address}`: the bus only listens on unix sockets."),
        );
    });
}

async fn greet_(socket_addr: &str) {
    let mut bus = Bus::for_address(Some(socket_addr)).await.unwrap();
    // The bus gives its clients the address that it was given, with its GUID added, and that's
    // where they connect.
    let address = bus.address().clone();
    let given = Address::try_from(socket_addr).unwrap();
    assert_eq!(address.transport(), given.transport());
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

    let ret = match greet_service(&address).await {
        Ok(service_conn) => greet_client(&address).await.map(|_| service_conn),
        Err(e) => Err(e),
    };
    stop.notify(usize::MAX);
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    let _ = ret.unwrap();
}

#[instrument]
async fn greet_service(address: &Address) -> anyhow::Result<Connection> {
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
    connection::Builder::address(address.clone())
        .name("org.zbus.MyGreeter")
        .serve_at("/org/zbus/MyGreeter", greeter)
        .build()
        .await
        .map_err(Into::into)
}

#[instrument]
async fn greet_client(address: &Address) -> anyhow::Result<()> {
    #[proxy(
        interface = "org.zbus.MyGreeter1",
        default_path = "/org/zbus/MyGreeter"
    )]
    trait MyGreeter {
        fn say_hello(&self, name: &str, unicast: bool) -> zbus::Result<String>;

        #[zbus(signal)]
        async fn greeted(name: &str, count: u64, path: ObjectPath<'_>);
    }

    let runtime = SharedRuntime::current()?;
    let conn = connection::Builder::address(address.clone())
        .build()
        .await?;

    let proxy = MyGreeterProxy::builder(&conn)
        .destination("org.zbus.MyGreeter")
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
    runtime
        .timeout(Duration::from_millis(10), msg_stream.next())
        .await
        .unwrap_err();

    // ..unless it's targeted to us specifically
    let _ = proxy.say_hello("Maria", true).await?;
    let signal = runtime
        .timeout(Duration::from_millis(10), msg_stream.next())
        .await?
        .unwrap();
    let args = signal.args()?;
    assert_eq!(args.name, "Maria");

    // Now let's try a manual subscription.
    let match_rule = MatchRule::builder()
        .interface("org.zbus.MyGreeter1")
        .member("Greeted")
        .add_arg("Maria")
        .arg_path(2, "/org/zbus/MyGreeter")
        .build()?;
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
