//! The command lines that start the brokers: split into words as a shell would split them, but
//! never run by one, so that each broker is a child of the runner, which reaps it and so learns
//! what it used.

use std::{
    env,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use crate::calls::Result;

/// The command line of a broker, split into words, with the placeholders of the socket still in
/// them.
///
/// `{address}` in a word stands for the address of the socket of a run, `unix:path=<socket>`, and
/// `{path}` for the path of the socket alone. The words before the program that look like
/// `NAME=value` set environment variables for the broker alone.
pub struct CommandLine {
    /// The words, as the command line has them.
    words: Vec<String>,
}

impl CommandLine {
    /// Splits `command` into words, and checks that it has a placeholder of the socket and names a
    /// program that can be run, on PATH or by its path.
    pub fn parse(command: &str) -> Result<Self> {
        if !command.contains("{address}") && !command.contains("{path}") {
            return Err(
                "it has neither {address} nor {path}, where the socket of a run goes".into(),
            );
        }
        let line = Self {
            words: split(command)?,
        };

        let (_, argv) = line.resolve(Path::new("/x"));
        let Some(program) = argv.first() else {
            return Err("it names no program".into());
        };
        if find_program(program).is_none() {
            return Err(format!("{program} is not a program that can be run").into());
        }

        Ok(line)
    }

    /// The environment variables, and the arguments, the program first, that start the broker on
    /// the socket at `socket`.
    pub fn resolve(&self, socket: &Path) -> (Vec<(String, String)>, Vec<String>) {
        let path = socket.to_string_lossy();
        let address = format!("unix:path={path}");
        let mut words = self
            .words
            .iter()
            .map(|word| word.replace("{address}", &address).replace("{path}", &path))
            .peekable();

        let mut env = Vec::new();
        while let Some(set) = words.peek().and_then(|word| variable(word)) {
            env.push(set);
            words.next();
        }

        (env, words.collect())
    }
}

/// `word` quoted for a command line, so that splitting the line gives `word` back: as it is if
/// there is nothing in it that splitting would change, and in single quotes otherwise.
pub fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    if plain {
        return word.to_owned();
    }

    // A single quote ends the quoted part, is kept by the backslash, and starts the next part.
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The path of the program `name`: `name` itself if it has a directory in it, and the first
/// directory of PATH with a program of that name otherwise, if there is one that can be run.
pub fn find_program(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);

        return is_executable(&path).then_some(path);
    }

    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

/// Splits `command` into words as a POSIX shell would, but with none of its expansions.
///
/// Blanks separate the words. A backslash keeps the character after it as it is, and so do single
/// quotes, everything up to the next one. Double quotes do too, but for a backslash before a double
/// quote or a backslash, which keeps that one as it is.
fn split(command: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    // `None` between two words, so that `''` makes an empty word.
    let mut word: Option<String> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_ascii_whitespace() => words.extend(word.take()),
            '\\' => {
                let Some(next) = chars.next() else {
                    return Err("it ends with a backslash that keeps nothing".into());
                };
                word.get_or_insert_default().push(next);
            }
            '\'' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a single quote in it is not closed".into()),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("a double quote in it is not closed".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("a double quote in it is not closed".into()),
                    }
                }
            }
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);

    Ok(words)
}

/// The environment variable that `word` sets, if it is of the form `NAME=value`.
fn variable(word: &str) -> Option<(String, String)> {
    let (name, value) = word.split_once('=')?;
    let mut chars = name.chars();
    let starts = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }

    Some((name.to_owned(), value.to_owned()))
}

/// Whether `path` is a file that its owner, its group or anyone else may run.
fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}
