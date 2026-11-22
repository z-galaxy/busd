#![cfg(feature = "testing")]

use anyhow::ensure;
use busd::testing::{connect, init_tracing, with_bus};
use futures_util::TryStreamExt;
use ntest::timeout;
use tracing::instrument;
use zbus::{message, MatchRule, Message, MessageStream};

const PATH: &str = "/org/zbus/Sender";
const INTERFACE: &str = "org.zbus.Sender";

/// A message of a client that leaves its sender field out, as most clients do, gets to its
/// recipient with the unique name of the client for its sender, along with its body and its file
/// descriptors.
#[test]
#[instrument]
#[timeout(30000)]
fn sender_left_out() {
    init_tracing();

    busd::testing::block_on(with_bus(sender_left_out_));
}

async fn sender_left_out_(address: String) -> anyhow::Result<()> {
    let recipient = connect(&address).await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    let mut signals = MessageStream::for_match_rule(rule, &recipient, None).await?;
    let sender = connect(&address).await?;
    let unique_name = sender.unique_name().unwrap();

    let signal = Message::signal(PATH, INTERFACE, "Body").build(&(7u32, "seven"))?;
    ensure!(
        signal.header().sender().is_none(),
        "the signal has a sender"
    );
    sender.send(&signal).await?;
    #[cfg(unix)]
    {
        let stdout = std::io::stdout();
        let signal =
            Message::signal(PATH, INTERFACE, "Fd").build(&(zbus::Fd::from(&stdout), 7u8))?;
        sender.send(&signal).await?;
    }

    let msg = signals.try_next().await?.unwrap();
    ensure!(
        msg.header().sender() == Some(unique_name),
        "the signal got to its recipient from {:?}",
        msg.header().sender(),
    );
    let body: (u32, String) = msg.body().deserialize()?;
    ensure!(body == (7, "seven".to_string()), "the body is {body:?}");
    #[cfg(unix)]
    {
        let msg = signals.try_next().await?.unwrap();
        ensure!(
            msg.header().sender() == Some(unique_name),
            "the signal got to its recipient from {:?}",
            msg.header().sender(),
        );
        let fds = msg.data().fds().len();
        ensure!(fds == 1, "the signal brought {fds} file descriptors");
    }

    Ok(())
}
