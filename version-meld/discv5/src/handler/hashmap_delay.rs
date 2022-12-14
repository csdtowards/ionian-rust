//! A simple hashmap object coupled with a `delay_queue` which has entries that expire after a
//! fixed time.
//!
//! A `HashMapDelay` implements `Stream` which removes expired items from the map.

/// The default delay for entries, in seconds. This is only used when `insert()` is used to add
/// entries.
const DEFAULT_DELAY: u64 = 30;

use futures::prelude::*;
use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio_util::time::delay_queue::{self, DelayQueue};

pub struct HashMapDelay<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + std::clone::Clone + Unpin,
{
    /// The given entries.
    entries: HashMap<K, MapEntry<V>>,
    /// A queue holding the timeouts of each entry.
    expirations: DelayQueue<K>,
    /// The default expiration timeout of an entry.
    default_entry_timeout: Duration,
}

/// A wrapping around entries that adds the link to the entry's expiration, via a `delay_queue` key.
struct MapEntry<V> {
    /// The expiration key for the entry.
    key: delay_queue::Key,
    /// The actual entry.
    value: V,
}

impl<K, V> Default for HashMapDelay<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + std::clone::Clone + Unpin,
{
    fn default() -> Self {
        HashMapDelay::new(Duration::from_secs(DEFAULT_DELAY))
    }
}

impl<K, V> HashMapDelay<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + std::clone::Clone + Unpin,
{
    /// Creates a new instance of `HashMapDelay`.
    pub fn new(default_entry_timeout: Duration) -> Self {
        HashMapDelay {
            entries: HashMap::new(),
            expirations: DelayQueue::new(),
            default_entry_timeout,
        }
    }

    /// Insert an entry into the mapping. Entries will expire after the `default_entry_timeout`.
    pub fn insert(&mut self, key: K, value: V) {
        self.insert_at(key, value, self.default_entry_timeout);
    }

    /// Inserts an entry that will expire at a given instant.
    pub fn insert_at(&mut self, key: K, value: V, entry_duration: Duration) {
        if self.contains_key(&key) {
            // update the timeout
            self.update_timeout(&key, value, entry_duration);
        } else {
            let delay_key = self.expirations.insert(key.clone(), entry_duration);
            let entry = MapEntry {
                key: delay_key,
                value,
            };
            self.entries.insert(key, entry);
        }
    }

    /// Updates the timeout for a given key. Returns true if the key existed, false otherwise.
    ///
    /// Panics if the duration is too far in the future.
    pub fn update_timeout(&mut self, key: &K, value: V, timeout: Duration) -> bool {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.value = value;
            self.expirations.reset(&entry.key, timeout);
            true
        } else {
            false
        }
    }

    /// Gets a reference to an entry if it exists.
    ///
    /// Returns None if the entry does not exist.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|entry| &entry.value)
    }

    /// Gets a mutable reference to an entry if it exists.
    ///
    /// Returns None if the entry does not exist.
    pub fn _get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries.get_mut(key).map(|entry| &mut entry.value)
    }

    /// Returns true if the key exists, false otherwise.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Returns the length of the mapping.
    pub fn _len(&self) -> usize {
        self.entries.len()
    }

    /// Removes a key from the map returning the value associated with the key that was in the map.
    ///
    /// Return None if the key was not in the map.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        if let Some(entry) = self.entries.remove(key) {
            self.expirations.remove(&entry.key);
            return Some(entry.value);
        }
        None
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// In other words, remove all pairs `(k, v)` such that `f(&k,&mut v)` returns false.
    pub fn _retain<F: FnMut(&K, &mut V) -> bool>(&mut self, mut f: F) {
        let expiration = &mut self.expirations;
        self.entries.retain(|key, entry| {
            let result = f(key, &mut entry.value);
            if !result {
                expiration.remove(&entry.key);
            }
            result
        })
    }

    /// Removes all entries from the map.
    pub fn _clear(&mut self) {
        self.entries.clear();
        self.expirations.clear();
    }
}

impl<K, V> Stream for HashMapDelay<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + std::clone::Clone + Unpin,
    V: Unpin,
{
    type Item = Result<(K, V), String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.expirations.poll_expired(cx) {
            Poll::Ready(Some(Ok(key))) => match self.entries.remove(key.get_ref()) {
                Some(entry) => Poll::Ready(Some(Ok((key.into_inner(), entry.value)))),
                None => Poll::Ready(Some(Err("Value no longer exists in expirations".into()))),
            },
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(format!("delay queue error: {:?}", e))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
