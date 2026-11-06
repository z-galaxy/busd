use std::{
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
};

/// The users of the bus, by uid.
///
/// The peers of a uid share one user, and the bus accounts the objects that it keeps for them by
/// user, as dbus-broker does, so that a uid can't have the bus keep more of them by opening more
/// connections.
#[derive(Debug)]
pub struct Users(Arc<Mutex<Registry>>);

impl Users {
    /// No users yet, each of which the bus may keep `max_objects` objects for.
    pub fn new(max_objects: usize) -> Self {
        Self(Arc::new(Mutex::new(Registry {
            users: HashMap::default(),
            max_objects,
        })))
    }

    /// The user of `uid`.
    pub fn user(&self, uid: u32) -> Arc<User> {
        let mut registry = lock(&self.0);
        if let Some(user) = registry.users.get(&uid).and_then(Weak::upgrade) {
            return user;
        }
        let user = Arc::new(User {
            uid,
            objects: Mutex::new(Objects {
                max: registry.max_objects,
                used: 0,
                shares: HashMap::default(),
            }),
            users: Arc::downgrade(&self.0),
        });
        registry.users.insert(uid, Arc::downgrade(&user));

        user
    }

    /// Let the bus keep `max` objects for each user, from now on.
    pub fn set_max_objects(&self, max: usize) {
        // The users are updated once the lock of the registry is let go of, as the last reference
        // to one of them may be among these, and a user takes the lock as it goes.
        let users: Vec<_> = {
            let mut registry = lock(&self.0);
            registry.max_objects = max;

            registry.users.values().filter_map(Weak::upgrade).collect()
        };
        for user in users {
            user.objects().max = max;
        }
    }
}

/// A user of the bus, and the account of the objects that the bus keeps for it.
#[derive(Debug)]
pub struct User {
    uid: u32,
    objects: Mutex<Objects>,
    /// The registry of the user, which it leaves as it goes.
    users: Weak<Mutex<Registry>>,
}

impl User {
    fn objects(&self) -> MutexGuard<'_, Objects> {
        lock(&self.objects)
    }
}

impl Drop for User {
    fn drop(&mut self) {
        let Some(registry) = self.users.upgrade() else {
            return;
        };
        let mut registry = lock(&registry);
        // Another user of the same uid may have taken the place of this one already.
        if let Entry::Occupied(user) = registry.users.entry(self.uid) {
            if user.get().strong_count() == 0 {
                user.remove();
            }
        }
    }
}

/// An object that the bus keeps for a user, on behalf of the same user or of another, which counts
/// against the objects of the first user for as long as it is kept.
#[derive(Debug)]
pub struct Charge {
    user: Arc<User>,
    actor: u32,
}

impl Charge {
    /// Charge an object to `user`, on behalf of `actor`, unless `actor` has as many of the objects
    /// of `user` as it may already.
    ///
    /// A user may have the bus keep as many objects of its own as it has left. Another user may
    /// only have it keep a share of those, as with dbus-broker: one more object, for a user that
    /// has `share` objects of `user` already, needs `(share + 1) * (n + 1) - share` objects or more
    /// left, where `n` is the number of users that have any objects of `user`, counting `actor` in.
    /// So, once charged, `actor` has at most 1/(n + 1) of the objects of `user` that no other user
    /// has: those left and its own.
    pub fn new(user: &Arc<User>, actor: &User) -> Option<Self> {
        let objects = user.objects();
        let left = objects.max.saturating_sub(objects.used);
        let share = objects.shares.get(&actor.uid).copied().unwrap_or(0);
        let allowed = if actor.uid == user.uid {
            left > 0
        } else {
            let users = objects.shares.len() + usize::from(share == 0);
            (share + 1)
                .checked_mul(users + 1)
                .is_some_and(|needed| needed - share <= left)
        };
        if !allowed {
            return None;
        }

        Some(Self::kept(user, actor, objects))
    }

    /// Charge an object to `user`, on behalf of `actor`, that another instance of the bus handed
    /// over, whatever the objects of `user` that `actor` has already.
    ///
    /// That instance kept all of the objects that it hands over at once, but it charged them in
    /// another order, and with others that went meanwhile, which the share that [`Charge::new`]
    /// allows `actor` depends on.
    #[cfg(target_os = "linux")]
    pub(crate) fn taken_over(user: &Arc<User>, actor: &User) -> Self {
        Self::kept(user, actor, user.objects())
    }

    /// Charge an object to `user`, on behalf of `actor`, in `objects`, the account of `user`.
    fn kept(user: &Arc<User>, actor: &User, mut objects: MutexGuard<'_, Objects>) -> Self {
        objects.used += 1;
        *objects.shares.entry(actor.uid).or_default() += 1;
        drop(objects);

        Self {
            user: user.clone(),
            actor: actor.uid,
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let mut objects = self.user.objects();
        objects.used -= 1;
        let Entry::Occupied(mut share) = objects.shares.entry(self.actor) else {
            unreachable!("a charge without a share");
        };
        *share.get_mut() -= 1;
        if *share.get() == 0 {
            share.remove();
        }
    }
}

/// How many objects the bus keeps for each user at most, by default: as many as dbus-broker keeps
/// for each (`--max-objects`), of its peers, the names that they own, the calls to them that wait
/// for replies, and others.
///
/// The bus counts the peers of each user, and the calls to them that wait for replies.
pub const MAX_OBJECTS: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct Registry {
    /// The users, which are kept for as long as anything holds them, by uid.
    users: HashMap<u32, Weak<User>, foldhash::fast::RandomState>,
    /// How many objects the bus keeps for each user at most.
    max_objects: usize,
}

/// The account of the objects that the bus keeps for a user.
#[derive(Debug)]
struct Objects {
    /// How many objects the bus keeps for the user at most.
    max: usize,
    /// How many objects the bus keeps for the user.
    used: usize,
    /// How many of them the bus keeps on behalf of each user, the user itself included, by uid.
    ///
    /// Only the users that have any are in here, so that its length counts them.
    shares: HashMap<u32, usize, foldhash::fast::RandomState>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing can panic while one of the locks is held.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_objects() {
        let users = Users::new(3);
        let user = users.user(1);

        let mut charges: Vec<_> = (0..3).map(|_| Charge::new(&user, &user).unwrap()).collect();
        assert!(Charge::new(&user, &user).is_none());
        charges.pop();
        assert!(Charge::new(&user, &user).is_some());
    }

    #[test]
    fn shares_of_other_users() {
        let users = Users::new(12);
        let user = users.user(1);
        let first = users.user(2);
        let second = users.user(3);
        // An object of its own, as each peer of a user is.
        let own = Charge::new(&user, &user).unwrap();

        // With 11 objects left, and the user and the first one with any, the first takes one more
        // as long as `(share + 1) * 3 - share` of them are left: 3, 5 and 7 for its first three,
        // but not 9 for a fourth, with 8 left.
        let firsts = charge_all(&user, &first);
        assert_eq!(firsts.len(), 3);
        // With 8 left, and 3 users with any, the second takes one more as long as
        // `(share + 1) * 4 - share` of them are left: 4 and 7 for its first two, but not 10 for a
        // third, with 6 left.
        let seconds = charge_all(&user, &second);
        assert_eq!(seconds.len(), 2);
        // The user itself takes all of those that are left.
        let owns = charge_all(&user, &user);
        assert_eq!(owns.len(), 6);
        assert!(Charge::new(&user, &first).is_none());

        // The objects of the first that go are left for the others, and it no longer counts.
        drop(firsts);
        assert_eq!(user.objects().shares.len(), 2);
        assert_eq!(charge_all(&user, &user).len(), 3);
        drop((own, seconds, owns));
        assert_eq!(user.objects().used, 0);
        assert!(user.objects().shares.is_empty());
    }

    #[test]
    fn first_share() {
        let users = Users::new(3);
        let user = users.user(1);
        let other = users.user(2);
        let _own = Charge::new(&user, &user).unwrap();

        // With 2 objects left, the first object of another user needs `(0 + 1) * (2 + 1) - 0` of
        // them, as the other one counts itself in with the user.
        assert!(Charge::new(&user, &other).is_none());
        assert!(!user.objects().shares.contains_key(&2));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn taken_over_whatever_the_share() {
        let users = Users::new(3);
        let user = users.user(1);
        let other = users.user(2);
        let _own = Charge::new(&user, &user).unwrap();
        assert!(Charge::new(&user, &other).is_none());

        // An object that another instance of the bus handed over counts, as any other, beyond the
        // share of `other`.
        {
            let _taken_over = Charge::taken_over(&user, &other);
            assert_eq!(user.objects().used, 2);
            assert_eq!(user.objects().shares.get(&2), Some(&1));
        }
        assert_eq!(user.objects().used, 1);
        assert!(!user.objects().shares.contains_key(&2));
    }

    #[test]
    fn registry() {
        let users = Users::new(1);
        let user = users.user(1);
        assert!(Arc::ptr_eq(&user, &users.user(1)));
        assert!(!Arc::ptr_eq(&user, &users.user(2)));

        // The maximum applies to the users that there are already, and to those after them.
        users.set_max_objects(2);
        assert_eq!(charge_all(&user, &user).len(), 2);
        let later = users.user(2);
        assert_eq!(charge_all(&later, &later).len(), 2);
        drop(later);

        // A user that goes leaves the registry.
        drop(user);
        assert!(lock(&users.0).users.is_empty());
    }

    /// Charge objects to `user` on behalf of `actor` until it may have no more.
    fn charge_all(user: &Arc<User>, actor: &User) -> Vec<Charge> {
        std::iter::from_fn(|| Charge::new(user, actor)).collect()
    }
}
