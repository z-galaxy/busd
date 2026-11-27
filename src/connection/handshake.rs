//! The server end of the handshake that opens a client's connection: the authentication protocol of
//! D-Bus, with the `EXTERNAL` mechanism only, as zbus's server goes through it.
//!
//! The reader of the client's messages runs the handshake, with `Reader::authenticate`: it reads
//! the client's lines, has the [`Server`] answer each, and writes the answers back.

use std::{io::IoSlice, os::fd::AsFd, str};

use zbus::{
    connection::handshake::{AuthMechanism, Command},
    OwnedGuid,
};
use zruntime::{net::unix::UnixStream, Local};

use super::socket;
use crate::trace;

/// The state of a client's handshake with the server of `guid`.
///
/// The server authenticates the user `uid`.
pub(super) struct Server<'g> {
    guid: &'g OwnedGuid,
    uid: u32,
    awaiting: Awaiting,
    /// Whether the next line is the client's first, which starts with a 0 byte.
    first: bool,
    /// Whether the client asked to pass file descriptors.
    fd_passing: bool,
}

impl<'g> Server<'g> {
    /// The server of `guid`, which authenticates the user `uid`, before the client's first line.
    pub(super) fn new(guid: &'g OwnedGuid, uid: u32) -> Self {
        Self {
            guid,
            uid,
            awaiting: Awaiting::Auth,
            first: true,
            fd_passing: false,
        }
    }

    /// The answer to `line`, the client's next line without the `\r\n` that ends it, none once
    /// the handshake is done.
    pub(super) fn answer_line(&mut self, line: &[u8]) -> zbus::Result<Option<Command>> {
        let command = match (self.first, line.split_first()) {
            (false, _) => line,
            // The client sends a 0 byte ahead of its first command. The byte is not part of the
            // command.
            (true, Some((0, command))) => command,
            (true, _) => {
                return Err(zbus::Error::Handshake(
                    "The first byte of the client isn't 0".to_string(),
                ))
            }
        };
        self.first = false;
        let command: Command = str::from_utf8(command)
            .map_err(|e| zbus::Error::Handshake(e.to_string()))?
            .parse()?;
        trace!("The client sent `{command}` in its handshake");

        self.answer(command)
    }

    /// Whether the client asked to pass file descriptors.
    pub(super) fn fd_passing(&self) -> bool {
        self.fd_passing
    }

    /// The answer to `command`, none once the handshake is done.
    fn answer(&mut self, command: Command) -> zbus::Result<Option<Command>> {
        let answer = match (self.awaiting, command) {
            (Awaiting::Auth, Command::Auth(Some(AuthMechanism::External), Some(id)))
            | (Awaiting::Data, Command::Data(Some(id))) => self.external(&id)?,
            (Awaiting::Auth, Command::Auth(Some(AuthMechanism::External), None)) => {
                self.awaiting = Awaiting::Data;

                Command::Data(None)
            }
            (Awaiting::Data, Command::Data(None)) => self.ok(),
            (Awaiting::Begin, Command::Begin) => return Ok(None),
            (Awaiting::Begin, Command::NegotiateUnixFD) => {
                self.fd_passing = true;

                Command::AgreeUnixFD
            }
            // `AUTH` without a mechanism asks for the mechanisms that the server supports, and
            // `REJECTED` lists them. `AUTH` of another mechanism gets the same answer.
            (Awaiting::Auth, Command::Auth(..))
            | (Awaiting::Auth | Awaiting::Begin, Command::Cancel | Command::Error(_)) => {
                self.rejected()
            }
            _ => Command::Error("Unsupported or misplaced command".to_string()),
        };

        Ok(Some(answer))
    }

    /// The answer to `id`, the uid that the client claims with `EXTERNAL`, in decimal.
    fn external(&mut self, id: &[u8]) -> zbus::Result<Command> {
        let id =
            str::from_utf8(id).map_err(|e| zbus::Error::Handshake(format!("Invalid ID: {e}")))?;
        let claimed: u32 = id
            .parse()
            .map_err(|e| zbus::Error::Handshake(format!("Invalid UID: {e}")))?;

        Ok(match claimed == self.uid {
            true => self.ok(),
            false => self.rejected(),
        })
    }

    /// `OK`: the client is authenticated, and the handshake waits for `BEGIN`.
    fn ok(&mut self) -> Command {
        self.awaiting = Awaiting::Begin;

        Command::Ok(self.guid.clone())
    }

    /// `REJECTED`, with the one mechanism that the server supports: the client may authenticate
    /// again.
    fn rejected(&mut self) -> Command {
        self.awaiting = Awaiting::Auth;

        Command::Rejected(AuthMechanism::External.as_str().into())
    }
}

/// What the handshake waits for.
#[derive(Clone, Copy, Debug)]
enum Awaiting {
    /// `AUTH`.
    Auth,
    /// The `DATA` of `AUTH EXTERNAL`, because the client sent no initial response with it.
    Data,
    /// `BEGIN`, since the client is authenticated, or `NEGOTIATE_UNIX_FD` before it.
    Begin,
}

/// Write `answer` to the client of `stream`, as a line.
pub(super) async fn write_answer(stream: &UnixStream<Local>, answer: &Command) -> zbus::Result<()> {
    let line = format!("{answer}\r\n");
    let mut written = 0;
    while written < line.len() {
        let rest = &line.as_bytes()[written..];
        written += stream
            .write_with(|stream| socket::send(stream.as_fd(), &[IoSlice::new(rest)], &[]))
            .await?;
    }

    Ok(())
}
