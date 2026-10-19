//! The play queue of a zone: an ordered list of items, a current index and
//! a repeat mode. The engine and the zone handle share one behind a mutex;
//! every change bumps `version` so clients can diff cheaply.

use serde::{Deserialize, Serialize};

/// One queued item. `uri` is what the [`crate::source::SourceFactory`]
/// opens: a local path today, a library track id or stream URL later. The
/// metadata is for display only; the engine never looks at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Unique within the queue, assigned on insertion (ignored on input).
    #[serde(default)]
    pub id: u64,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl QueueItem {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            id: 0,
            uri: uri.into(),
            title: None,
            artist: None,
            album: None,
            duration_ms: None,
        }
    }

    /// "Artist – Title", or the URI when untitled.
    pub fn label(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(a), Some(t)) => format!("{a} – {t}"),
            (None, Some(t)) => t.clone(),
            _ => self.uri.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// Where [`Queue::add`] puts new items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    End,
    /// Right after the current item (or at the front when nothing is current).
    Next,
}

#[derive(Debug, Clone, Default)]
pub struct Queue {
    items: Vec<QueueItem>,
    current: Option<usize>,
    next_id: u64,
    repeat: Repeat,
    version: u64,
}

impl Queue {
    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn set_repeat(&mut self, r: Repeat) {
        if self.repeat != r {
            self.repeat = r;
            self.version += 1;
        }
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.current.and_then(|i| self.items.get(i))
    }

    pub fn get(&self, index: usize) -> Option<&QueueItem> {
        self.items.get(index)
    }

    pub fn position_of(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    /// Point at `index`; false when out of range.
    pub fn set_current(&mut self, index: Option<usize>) -> bool {
        if let Some(i) = index {
            if i >= self.items.len() {
                return false;
            }
        }
        if self.current != index {
            self.current = index;
            self.version += 1;
        }
        true
    }

    /// Insert items (ids are assigned here) and return their ids.
    pub fn add(&mut self, items: Vec<QueueItem>, at: Insert) -> Vec<u64> {
        if items.is_empty() {
            return Vec::new();
        }
        let mut ids = Vec::with_capacity(items.len());
        let items: Vec<QueueItem> = items
            .into_iter()
            .map(|mut it| {
                self.next_id += 1;
                it.id = self.next_id;
                ids.push(it.id);
                it
            })
            .collect();
        let at = match at {
            Insert::End => self.items.len(),
            Insert::Next => self.current.map(|c| c + 1).unwrap_or(0),
        };
        self.items.splice(at..at, items);
        self.version += 1;
        ids
    }

    /// Remove one item. The current index follows its item; removing the
    /// current item leaves the index on whatever moved into its slot, or
    /// clears it at the end of the queue.
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(i) = self.position_of(id) else {
            return false;
        };
        self.items.remove(i);
        self.current = match self.current {
            Some(c) if c > i => Some(c - 1),
            Some(c) if c == i && i >= self.items.len() => None,
            other => other,
        };
        self.version += 1;
        true
    }

    /// Move the item `id` to `to_index` (clamped). The current index follows its item.
    pub fn move_item(&mut self, id: u64, to_index: usize) -> bool {
        let Some(from) = self.position_of(id) else {
            return false;
        };
        let to = to_index.min(self.items.len() - 1);
        if from == to {
            return true;
        }
        let current_id = self.current().map(|c| c.id);
        let it = self.items.remove(from);
        self.items.insert(to, it);
        self.current = current_id.and_then(|cid| self.position_of(cid));
        self.version += 1;
        true
    }

    pub fn clear(&mut self) {
        if !self.items.is_empty() || self.current.is_some() {
            self.items.clear();
            self.current = None;
            self.version += 1;
        }
    }

    /// What plays when the item `played_id` ends naturally. If that item was
    /// removed meanwhile, the item that moved into its slot plays next.
    pub fn after_end(&self, played_id: u64) -> Option<usize> {
        let n = self.items.len();
        if n == 0 {
            return None;
        }
        let Some(i) = self.position_of(played_id) else {
            return self.current.filter(|c| *c < n);
        };
        match self.repeat {
            Repeat::One => Some(i),
            Repeat::All => Some((i + 1) % n),
            Repeat::Off => (i + 1 < n).then_some(i + 1),
        }
    }

    /// What plays on an explicit "next": repeat-one is ignored, repeat-all wraps.
    pub fn after_skip(&self) -> Option<usize> {
        let n = self.items.len();
        let Some(c) = self.current else {
            return (n > 0).then_some(0);
        };
        match self.repeat {
            Repeat::All => Some((c + 1) % n),
            _ => (c + 1 < n).then_some(c + 1),
        }
    }

    /// The item before the current one, wrapping under repeat-all. `None`
    /// means "restart the current track".
    pub fn before(&self) -> Option<usize> {
        let n = self.items.len();
        let c = self.current?;
        match (c, self.repeat) {
            (0, Repeat::All) if n > 0 => Some(n - 1),
            (0, _) => None,
            (c, _) => Some(c - 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(n: usize) -> Queue {
        let mut q = Queue::default();
        q.add(
            (0..n).map(|i| QueueItem::new(format!("t{i}"))).collect(),
            Insert::End,
        );
        q
    }

    #[test]
    fn add_assigns_ids_and_positions() {
        let mut q = q(2);
        assert_eq!(q.items().iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2]);
        q.set_current(Some(0));
        let ids = q.add(vec![QueueItem::new("n")], Insert::Next);
        assert_eq!(ids, [3]);
        assert_eq!(q.items()[1].uri, "n");
        assert_eq!(q.current_index(), Some(0));
        let v = q.version();
        assert_eq!(q.add(vec![], Insert::End), Vec::<u64>::new());
        assert_eq!(q.version(), v);
    }

    #[test]
    fn remove_keeps_current_on_its_item() {
        let mut q = q(4);
        q.set_current(Some(2));
        assert!(q.remove(1));
        assert_eq!(q.current_index(), Some(1));
        assert_eq!(q.current().unwrap().uri, "t2");
        // Remove the current item: index stays, now on t3.
        assert!(q.remove(3));
        assert_eq!(q.current().unwrap().uri, "t3");
        // Remove the last, current item: cleared.
        assert!(q.remove(4));
        assert_eq!(q.current_index(), None);
        assert!(!q.remove(99));
    }

    #[test]
    fn move_follows_current() {
        let mut q = q(3);
        q.set_current(Some(2));
        assert!(q.move_item(3, 0));
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.items()[0].uri, "t2");
        assert!(q.move_item(1, 10));
        assert_eq!(q.items()[2].uri, "t0");
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn after_end_respects_repeat_and_removed_items() {
        let mut q = q(3);
        q.set_current(Some(2));
        assert_eq!(q.after_end(3), None);
        q.set_repeat(Repeat::All);
        assert_eq!(q.after_end(3), Some(0));
        q.set_repeat(Repeat::One);
        assert_eq!(q.after_end(3), Some(2));
        q.set_repeat(Repeat::Off);
        q.set_current(Some(0));
        // Playing item 1 (index 0); it is removed while playing: t1 is now at 0 and plays next.
        q.remove(1);
        assert_eq!(q.after_end(1), Some(0));
        q.clear();
        assert_eq!(q.after_end(2), None);
    }

    #[test]
    fn skip_and_previous() {
        let mut q = q(2);
        assert_eq!(q.after_skip(), Some(0));
        q.set_current(Some(1));
        assert_eq!(q.after_skip(), None);
        assert_eq!(q.before(), Some(0));
        q.set_repeat(Repeat::All);
        assert_eq!(q.after_skip(), Some(0));
        q.set_current(Some(0));
        assert_eq!(q.before(), Some(1));
        q.set_repeat(Repeat::Off);
        assert_eq!(q.before(), None);
    }
}
