//! The ids of the users and groups that the `user` and `group` attributes of a configuration name.

use std::{collections::HashMap, fs::read_to_string};

use crate::warn;

/// The ids of the users and groups of the system, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ids {
    users: HashMap<String, u32>,
    groups: HashMap<String, u32>,
}

impl Ids {
    /// The users of `/etc/passwd` and the groups of `/etc/group`. A file that can't be read gives
    /// no names, with a warning.
    ///
    /// This reads the files with blocking calls, so a caller that serves a bus meanwhile runs it
    /// with `zruntime::unblock`.
    ///
    /// It reads these two files and nothing else: it does not ask NSS, so the users and groups
    /// that only a directory service knows, such as those of LDAP, of sssd or the dynamic users of
    /// systemd, do not resolve.
    pub fn read() -> Self {
        Self::parse(&read_or_warn(PASSWD), &read_or_warn(GROUP))
    }

    /// The users of `passwd` and the groups of `group`, the text of a passwd file and of a group
    /// file.
    ///
    /// A line of the passwd file is `name:password:uid:gid:...`, and one of the group file
    /// `name:password:gid:members`. Empty lines, comments (`#`), the NIS lines that start with `+`
    /// or `-`, and lines that do not parse are skipped. Where several lines have one name, the
    /// first one counts, as it does for the C library.
    pub fn parse(passwd: &str, group: &str) -> Self {
        Self {
            users: parse_ids(passwd),
            groups: parse_ids(group),
        }
    }

    /// The uid of `user`: the number itself if `user` is one, or else that of the user so named.
    pub fn uid(&self, user: &str) -> Option<u32> {
        number(user).or_else(|| self.users.get(user).copied())
    }

    /// The gid of `group`: the number itself if `group` is one, or else that of the group so
    /// named.
    pub fn gid(&self, group: &str) -> Option<u32> {
        number(group).or_else(|| self.groups.get(group).copied())
    }
}

const PASSWD: &str = "/etc/passwd";
const GROUP: &str = "/etc/group";

/// The text of the file at `path`, or nothing if it can't be read.
fn read_or_warn(path: &str) -> String {
    match read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            warn!("Cannot read `{path}`, so none of its names resolve: {e}");
            String::new()
        }
    }
}

/// The ids of the names of `text`, whose lines have a name as their first field and an id as
/// their third, as both passwd and group files do.
fn parse_ids(text: &str) -> HashMap<String, u32> {
    let mut ids = HashMap::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with(['#', '+', '-']) {
            continue;
        }
        let mut fields = line.split(':');
        let (Some(name), Some(_password), Some(id)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(id) = number(id) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        ids.entry(name.to_string()).or_insert(id);
    }

    ids
}

/// The number that `s` is, if it is all digits, and the number fits in an id.
fn number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names of the users and the groups resolve to the ids of their lines.
    #[test]
    fn names_resolve_to_their_ids() {
        let ids = Ids::parse(USERS, GROUPS);

        assert_eq!(ids.uid("root"), Some(0));
        assert_eq!(ids.uid("messagebus"), Some(101));
        assert_eq!(ids.uid("alice"), Some(1000));
        assert_eq!(ids.gid("root"), Some(0));
        assert_eq!(ids.gid("wheel"), Some(10));
        assert_eq!(ids.gid("alice"), Some(1000));
    }

    /// Users and groups have names of their own: a group's name is no user's, and the other way
    /// round.
    #[test]
    fn users_and_groups_do_not_mix() {
        let ids = Ids::parse(USERS, GROUPS);

        assert_eq!(ids.uid("wheel"), None);
        assert_eq!(ids.gid("messagebus"), None);
    }

    /// A value that is all digits is the id itself, whether a line has it or not, and one that
    /// is not all digits, or is too large for an id, is a name.
    #[test]
    fn numbers_are_ids() {
        let ids = Ids::parse(USERS, GROUPS);

        assert_eq!(ids.uid("0"), Some(0));
        assert_eq!(ids.uid("4242"), Some(4242));
        assert_eq!(ids.gid("4242"), Some(4242));
        assert_eq!(ids.uid("4294967295"), Some(u32::MAX));
        assert_eq!(ids.uid("4294967296"), None);
        assert_eq!(ids.uid("+5"), None);
        assert_eq!(ids.uid("-5"), None);
        assert_eq!(ids.uid(""), None);
    }

    /// A name that no line has does not resolve.
    #[test]
    fn unknown_names_do_not_resolve() {
        let ids = Ids::parse(USERS, GROUPS);

        assert_eq!(ids.uid("bob"), None);
        assert_eq!(ids.gid("bob"), None);
    }

    /// Empty lines, comments, NIS lines and lines that do not parse are skipped, and the lines
    /// around them still count.
    #[test]
    fn lines_that_are_no_entries_are_skipped() {
        let passwd = "
# carol:x:1001:1001::/home/carol:/bin/sh
+dave:x:1002:1002::/home/dave:/bin/sh
-erin:x:1003:1003::/home/erin:/bin/sh
+
frank
grace:x
heidi:x:not-a-number:1004::/home/heidi:/bin/sh
ivan:x:+1005:1005::/home/ivan:/bin/sh
:x:1006:1006::/:/bin/sh
judy:x:1007:1007::/home/judy:/bin/sh
";
        let group = "#staff:x:50:\n+nis\nstaff:x:\nusers:x:100:judy\n";
        let ids = Ids::parse(passwd, group);

        for name in [
            "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "",
        ] {
            assert_eq!(ids.uid(name), None, "{name:?} should not resolve");
        }
        assert_eq!(ids.uid("judy"), Some(1007));
        assert_eq!(ids.gid("#staff"), None);
        assert_eq!(ids.gid("staff"), None);
        assert_eq!(ids.gid("users"), Some(100));
    }

    /// Where several lines have one name, the first of them counts.
    #[test]
    fn the_first_line_of_a_name_counts() {
        let ids = Ids::parse(
            "alice:x:1000:1000::/home/alice:/bin/sh\nalice:x:0:0::/root:/bin/sh\n",
            "wheel:x:10:\nwheel:x:0:\n",
        );

        assert_eq!(ids.uid("alice"), Some(1000));
        assert_eq!(ids.gid("wheel"), Some(10));
    }

    /// A passwd file.
    const USERS: &str = "\
root:x:0:0:root:/root:/bin/bash
messagebus:x:101:104::/nonexistent:/usr/sbin/nologin
alice:x:1000:1000:Alice:/home/alice:/bin/bash
";
    /// A group file.
    const GROUPS: &str = "\
root:x:0:
wheel:x:10:alice
alice:x:1000:
";
}
