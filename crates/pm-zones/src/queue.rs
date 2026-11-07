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
    /// Cover image URL for items that are not library tracks (streaming sources).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Where fetching it may go, when someone other than the owner queued it (a listener, a
    /// UPnP controller); none for the owner's. Kept with the item: moved, saved and restored
    /// with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<FetchLimit>,
    /// Who queued it, when not the owner: a listener's Pubky key. A streaming service plays it
    /// with that person's own account, never anyone else's. Kept with the item: moved, saved,
    /// restored and taken into a listen-along group with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// Where the node may connect to fetch an item for someone other than the owner: addresses that
/// someone at `peer` could reach (loopback only for a peer on the node's own machine), LAN
/// addresses only with `lan` (see `pm_renderer::ip_allowed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchLimit {
    #[serde(default)]
    pub peer: Option<std::net::IpAddr>,
    #[serde(default)]
    pub lan: bool,
}

impl FetchLimit {
    /// A listener's: public addresses only.
    pub const PUBLIC: FetchLimit = FetchLimit {
        peer: None,
        lan: false,
    };

    /// The stricter of two.
    pub fn and(self, other: FetchLimit) -> FetchLimit {
        FetchLimit {
            peer: (self.peer == other.peer).then_some(self.peer).flatten(),
            lan: self.lan && other.lan,
        }
    }
}

/// The path of a link on the node: a relative `/v1/…` link, or an absolute one on one of the
/// node's own addresses (`origins`, e.g. `http://192.168.1.5:7788`). A `/v1/…` link on any other
/// host is someone else's and is left alone.
pub fn node_link_path<'a>(link: &'a str, origins: &[String]) -> Option<&'a str> {
    if link.starts_with("/v1/") {
        return Some(link);
    }
    origins.iter().find_map(|o| {
        let o = o.trim_end_matches('/');
        let head = link.get(..o.len())?;
        (!o.is_empty() && head.eq_ignore_ascii_case(o))
            .then(|| &link[o.len()..])
            .filter(|rest| rest.starts_with("/v1/"))
    })
}

/// Whether a link is a `/v1/…` link on some other host (`http(s)://host/v1/…`): maybe another
/// node's, maybe this node under a name it doesn't know as its own (`localhost`, a tunnel name).
fn other_hosts_v1_link(link: &str) -> bool {
    let Some(scheme) = ["http://", "https://"]
        .iter()
        .find(|p| link.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)))
    else {
        return false;
    };
    let rest = &link[scheme.len()..];
    match rest.find(['/', '?', '#']) {
        Some(i) if rest.as_bytes()[i] == b'/' => rest[i..].starts_with("/v1/"),
        _ => false,
    }
}

/// The query of a `/v1/` link (where it starts, where it ends: `#` or the end), and whether the
/// link is the node's own ([`node_link_path`]) rather than one on some other host.
fn v1_link_query(link: &str, origins: &[String]) -> Option<(usize, usize, bool)> {
    let ours = node_link_path(link, origins).is_some();
    if !ours && !other_hosts_v1_link(link) {
        return None;
    }
    let path_at = if link.starts_with('/') {
        0
    } else {
        let scheme = if link.get(..8).is_some_and(|h| h.eq_ignore_ascii_case("https://")) {
            8
        } else {
            7
        };
        scheme + link[scheme..].find('/')?
    };
    let q = path_at + link[path_at..].find('?')?;
    let end = link[q..].find('#').map_or(link.len(), |h| q + h);
    Some((q, end, ours))
}

/// A sign-in the node knows as one of its own: the API token, an owner's browser or device
/// session, a listener's token. Given by the node (`Zones::set_token_check`).
pub type TokenCheck = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// A queue's [`TokenCheck`], when the node gave one.
#[derive(Clone, Default)]
struct Check(Option<TokenCheck>);

impl std::fmt::Debug for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Check(set)"
        } else {
            "Check(none)"
        })
    }
}

/// The `token` parameters a `/v1/` link loses: every one on the node's own links, and on a
/// `/v1/` link of any other host those the node knows as its own sign-ins (`known`: a cover link
/// made under a name the node doesn't list as its own, `localhost`, say, still carries the
/// node's sign-in). A link on another host with a token the node doesn't know is someone
/// else's and is left alone.
fn tokens_to_drop(
    link: &str,
    origins: &[String],
    known: &dyn Fn(&str) -> bool,
) -> Option<(usize, usize, Vec<String>)> {
    let (q, end, ours) = v1_link_query(link, origins)?;
    let tokens: Vec<String> = link[q + 1..end].split('&').filter_map(token_param).collect();
    if tokens.is_empty() || (!ours && !tokens.iter().any(|t| known(t))) {
        return None;
    }
    Some((q, end, tokens))
}

/// The value of a `token` query parameter, read the way the node reads its query (form
/// decoding, so `%74oken=` counts as `token` and `token=a%2Bb` holds `a+b`); `None` for any
/// other parameter. Any case counts: the node reads only `token`, but the value is a sign-in
/// whatever the key's case.
fn token_param(p: &str) -> Option<String> {
    form_urlencoded::parse(p.as_bytes())
        .next()
        .filter(|(k, _)| k.eq_ignore_ascii_case("token"))
        .map(|(_, v)| v.into_owned())
}

/// Every `token` query parameter's value (decoded) that a `/v1/` link loses (`tokens_to_drop`):
/// all of them on the node's own links, the node's own sign-ins on another host's.
pub fn node_link_tokens(link: &str, origins: &[String], known: &dyn Fn(&str) -> bool) -> Vec<String> {
    tokens_to_drop(link, origins, known).map_or_else(Vec::new, |(_, _, t)| t)
}

/// A `/v1/` link without the `token` query parameters it loses (`tokens_to_drop`); `None` when
/// it loses none. The node's own links never carry a sign-in: a zone's queue is read by everyone
/// who may use the zone, saved across restarts and shown by renderers.
pub fn node_link_without_token(
    link: &str,
    origins: &[String],
    known: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let (q, end, _) = tokens_to_drop(link, origins, known)?;
    let params: Vec<&str> = link[q + 1..end].split('&').collect();
    let kept: Vec<&str> = params
        .into_iter()
        .filter(|p| !p.is_empty() && token_param(p).is_none())
        .collect();
    let mut out = link[..q].to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out.push_str(&link[end..]);
    Some(out)
}

impl QueueItem {
    /// Drops the `token` query parameters the item's links lose (`uri`, `image`;
    /// [`node_link_without_token`]). Whether anything changed.
    pub fn without_node_token(&mut self, origins: &[String], known: &dyn Fn(&str) -> bool) -> bool {
        let mut changed = false;
        if let Some(u) = node_link_without_token(&self.uri, origins, known) {
            self.uri = u;
            changed = true;
        }
        if let Some(i) = self
            .image
            .as_deref()
            .and_then(|i| node_link_without_token(i, origins, known))
        {
            self.image = Some(i);
            changed = true;
        }
        changed
    }

    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            id: 0,
            uri: uri.into(),
            title: None,
            artist: None,
            album: None,
            duration_ms: None,
            image: None,
            limit: None,
            by: None,
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

/// One consistent look at what a queue keeps across a restart ([`Queue::snapshot`]).
#[derive(Debug, Clone, PartialEq)]
pub struct QueueSnapshot {
    /// The play order.
    pub items: Vec<QueueItem>,
    pub repeat: Repeat,
    pub radio: bool,
    pub shuffle: bool,
    /// With shuffle on: the original order as indexes into `items` ([`Queue::shuffle_order`]).
    pub order: Vec<usize>,
    pub version: u64,
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
    /// The node's own addresses ([`node_link_path`]): links there lose their sign-in on the way in.
    node_origins: Vec<String>,
    /// The node's own sign-ins ([`TokenCheck`]): a link on any other host loses those.
    token_check: Check,
    items: Vec<QueueItem>,
    current: Option<usize>,
    next_id: u64,
    repeat: Repeat,
    /// "Radio": whoever owns the zone keeps it playing with similar music
    /// after the last item (the engine only carries the flag).
    radio: bool,
    /// Shuffle mode. `items` is always the play order; while shuffle is on, `order` keeps the ids
    /// in their original order, so turning it off puts every item back.
    shuffle: bool,
    order: Vec<u64>,
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

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn radio(&self) -> bool {
        self.radio
    }

    pub fn set_radio(&mut self, on: bool) {
        if self.radio != on {
            self.radio = on;
            self.version += 1;
        }
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

    /// The node's own addresses (`Zones::set_node_origins`).
    pub fn set_node_origins(&mut self, origins: Vec<String>) {
        self.node_origins = origins;
    }

    /// The node's own sign-ins (`Zones::set_token_check`).
    pub fn set_token_check(&mut self, check: Option<TokenCheck>) {
        self.token_check = Check(check);
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
                // Every way into a queue passes here: a node link never keeps a sign-in
                let known = |t: &str| self.token_check.0.as_ref().is_some_and(|f| f(t));
                it.without_node_token(&self.node_origins, &known);
                it
            })
            .collect();
        let pos = match at {
            Insert::End => self.items.len(),
            Insert::Next => self.current.map(|c| c + 1).unwrap_or(0),
        };
        self.items.splice(pos..pos, items);
        if self.shuffle {
            // The same place in the original order: "Play next" is next there too; "Add to
            // queue" goes to its end
            let opos = match at {
                Insert::End => self.order.len(),
                Insert::Next => self
                    .current
                    .and_then(|c| self.items.get(c))
                    .and_then(|cur| self.order.iter().position(|id| *id == cur.id))
                    .map(|p| p + 1)
                    .unwrap_or(0),
            };
            self.order.splice(opos..opos, ids.iter().copied());
        }
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
        self.order.retain(|x| *x != id);
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

    /// Shuffle the items after the current one (all items when nothing is
    /// current) with a Fisher–Yates pass driven by `seed`. The current item
    /// and everything already played stay where they are. Returns whether
    /// the order could change at all (at least two upcoming items).
    pub fn shuffle_upcoming(&mut self, seed: u64) -> bool {
        let start = self.current.map(|c| c + 1).unwrap_or(0);
        if !self.fisher_yates(start, seed) {
            return false;
        }
        self.version += 1;
        true
    }

    /// A Fisher–Yates pass over `items[start..]`, driven by `seed`; whether the order could
    /// change at all (at least two items there). The version is the caller's to bump.
    fn fisher_yates(&mut self, start: usize, seed: u64) -> bool {
        let start = start.min(self.items.len());
        let part = &mut self.items[start..];
        if part.len() < 2 {
            return false;
        }
        let mut x = seed | 1;
        for i in (1..part.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let j = (x % (i as u64 + 1)) as usize;
            part.swap(i, j);
        }
        true
    }

    /// Shuffle mode on or off; whether it changed. On: the original order is kept and the
    /// upcoming items are shuffled (the current item and everything played stay). Off: every
    /// item goes back to its original order, the current item following its id.
    pub fn set_shuffle(&mut self, on: bool, seed: u64) -> bool {
        if self.shuffle == on {
            return false;
        }
        if on {
            self.order = self.items.iter().map(|i| i.id).collect();
            self.shuffle = true;
            let start = self.current.map(|c| c + 1).unwrap_or(0);
            self.fisher_yates(start, seed);
        } else {
            let current_id = self.current().map(|c| c.id);
            let mut rest = std::mem::take(&mut self.items);
            let mut back = Vec::with_capacity(rest.len());
            for id in self.order.drain(..) {
                if let Some(i) = rest.iter().position(|it| it.id == id) {
                    back.push(rest.remove(i));
                }
            }
            // Anything the original order doesn't name (it can't happen; kept all the same)
            back.append(&mut rest);
            self.items = back;
            self.current = current_id.and_then(|cid| self.position_of(cid));
            self.shuffle = false;
        }
        self.version += 1;
        true
    }

    /// Shuffle again with the current item first and every other item shuffled after it:
    /// a queue loaded with shuffle on (the chosen item stays first), or "shuffle" pressed while
    /// on. Turns shuffle on when it was off.
    pub fn reshuffle(&mut self, seed: u64) {
        if !self.shuffle {
            self.order = self.items.iter().map(|i| i.id).collect();
            self.shuffle = true;
        }
        if let Some(c) = self.current.filter(|c| *c < self.items.len()) {
            let it = self.items.remove(c);
            self.items.insert(0, it);
            self.current = Some(0);
        }
        let start = self.current.map(|_| 1).unwrap_or(0);
        self.fisher_yates(start, seed);
        self.version += 1;
    }

    /// With shuffle on: the original order, as indexes into the play order (for saving; ids
    /// change when a saved queue is loaded again). `None` while shuffle is off.
    pub fn shuffle_order(&self) -> Option<Vec<usize>> {
        if !self.shuffle {
            return None;
        }
        Some(self.order.iter().filter_map(|id| self.position_of(*id)).collect())
    }

    /// Shuffle on again from a saved queue, with [`Self::shuffle_order`]'s indexes as the
    /// original order. Anything that isn't a permutation of the items makes the play order the
    /// original one. The play order stays as it is.
    pub fn restore_shuffle(&mut self, order: &[usize]) {
        let mut seen = order.to_vec();
        seen.sort_unstable();
        let valid = seen.len() == self.items.len() && seen.iter().enumerate().all(|(i, p)| *p == i);
        self.order = if valid {
            order.iter().map(|i| self.items[*i].id).collect()
        } else {
            self.items.iter().map(|i| i.id).collect()
        };
        self.shuffle = true;
        self.version += 1;
    }

    /// Everything that is saved per zone, from one look at the queue (the items in play order,
    /// the modes, and the original order as indexes while shuffled), with its version.
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            items: self.items.clone(),
            repeat: self.repeat,
            radio: self.radio,
            shuffle: self.shuffle,
            order: self.shuffle_order().unwrap_or_default(),
            version: self.version,
        }
    }

    /// Empties the queue; the shuffle and repeat modes stay.
    pub fn clear(&mut self) {
        if !self.items.is_empty() || self.current.is_some() {
            self.items.clear();
            self.order.clear();
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
    fn node_links_lose_their_token_on_the_way_in() {
        let me = vec!["http://192.168.1.5:7788/".to_string()];
        let no = |_: &str| false;
        let t = |l: &str| node_link_without_token(l, &me, &no);
        assert_eq!(
            t("/v1/library/albums/7/cover?token=pmo_abc").as_deref(),
            Some("/v1/library/albums/7/cover")
        );
        assert_eq!(
            t("HTTP://192.168.1.5:7788/v1/library/albums/7/cover?token=s3cret#x").as_deref(),
            Some("HTTP://192.168.1.5:7788/v1/library/albums/7/cover#x")
        );
        assert_eq!(
            t("/v1/sources/tidal/stream/9?q=1&token=t&k=2").as_deref(),
            Some("/v1/sources/tidal/stream/9?q=1&k=2")
        );
        // Every one of them (the last is the one a server reads)
        assert_eq!(t("/v1/x?token=a&b=1&token=b").as_deref(), Some("/v1/x?b=1"));
        assert_eq!(
            node_link_tokens("/v1/x?token=a&b=1&token=b", &me, &no),
            ["a", "b"]
        );
        assert_eq!(t("/v1/x?token").as_deref(), Some("/v1/x"));
        assert_eq!(node_link_tokens("/v1/x?a=1&token=pml_z", &me, &no), ["pml_z"]);
        // Read as the node reads its query: an encoded key, an encoded or `+` value, any case,
        // repeated; every other parameter stays byte for byte as it was
        let odd = "/v1/x?a=%2F+b&%74oken=p%6Do_1&T%4FKEN=two+3&b=%74ok&token=pmo%5F4#f";
        assert_eq!(t(odd).as_deref(), Some("/v1/x?a=%2F+b&b=%74ok#f"));
        assert_eq!(node_link_tokens(odd, &me, &no), ["pmo_1", "two 3", "pmo_4"]);
        assert_eq!(t("/v1/x?%74%6F%6B%65%6E=a").as_deref(), Some("/v1/x"));
        // Not `token` once decoded: kept
        assert_eq!(t("/v1/x?tok+en=a&token%3D=b&tokens=c"), None);
        // Not the node's links, or no token: untouched. Another node's or a service's `/v1/`
        // link keeps its own token
        for l in [
            "track:1",
            "/v1/library/albums/7/cover",
            "https://cdn.example/img.jpg?token=theirs",
            "https://other-node.example/v1/library/tracks/1/file?token=theirs",
            "http://192.168.1.6:7788/v1/x?token=theirs",
            "http://192.168.1.5:77880/v1/x?token=theirs",
            "file:///v1/x?token=a",
            "/music/a.flac?token=a",
            "/v1/x?mytoken=a",
        ] {
            assert_eq!(t(l), None, "{l}");
            assert!(node_link_tokens(l, &me, &no).is_empty(), "{l}");
        }
        assert_eq!(node_link_path("http://192.168.1.5:7788/v1/a", &me), Some("/v1/a"));
        assert_eq!(node_link_path("http://192.168.1.5:7788/v1/a", &[]), None);
        // A `/v1/` link on a host the node doesn't list as its own (the node under another name,
        // another node) loses the sign-ins this node knows as its own, and only those; a host
        // without a `/v1/` path is never looked at
        let mine = |t: &str| t == "pmo_mine" || t == "s3cret";
        let k = |l: &str| node_link_without_token(l, &me, &mine);
        assert_eq!(
            k("http://localhost:7788/v1/library/albums/7/cover?token=pmo_mine").as_deref(),
            Some("http://localhost:7788/v1/library/albums/7/cover")
        );
        assert_eq!(
            k("https://node.example.ts.net/v1/library/tracks/1/file?a=1&token=s3cret&token=theirs#x")
                .as_deref(),
            Some("https://node.example.ts.net/v1/library/tracks/1/file?a=1#x")
        );
        assert_eq!(
            node_link_tokens(
                "http://localhost:7788/v1/x?token=pmo_mine&token=theirs",
                &me,
                &mine
            ),
            ["pmo_mine", "theirs"]
        );
        for l in [
            "http://localhost:7788/v1/x?token=theirs",
            "https://cdn.example/v1.jpg?token=pmo_mine",
            "https://cdn.example?token=pmo_mine",
            "https://cdn.example/x/v1/y?token=pmo_mine",
            "/music/v1/a.flac?token=pmo_mine",
        ] {
            assert_eq!(k(l), None, "{l}");
            assert!(node_link_tokens(l, &me, &mine).is_empty(), "{l}");
        }
        let mut q = Queue::default();
        q.set_node_origins(me.clone());
        q.set_token_check(Some(std::sync::Arc::new(|t: &str| t == "pmo_mine")));
        let mut it = QueueItem::new("http://192.168.1.5:7788/v1/library/tracks/1/file?token=pmo_abc");
        it.image = Some("/v1/library/albums/7/cover?token=pmo_abc".into());
        let mut other = QueueItem::new("https://radio.example/v1/live?token=theirs");
        other.image = Some("https://cdn.example/a.jpg".into());
        let mut elsewhere = QueueItem::new("http://localhost:7788/v1/library/tracks/2/file?token=pmo_mine");
        elsewhere.image = Some("https://other-node.example/v1/library/albums/1/cover?token=pmo_mine".into());
        q.add(vec![it, other.clone(), elsewhere], Insert::End);
        assert_eq!(q.items()[2].uri, "http://localhost:7788/v1/library/tracks/2/file");
        assert_eq!(
            q.items()[2].image.as_deref(),
            Some("https://other-node.example/v1/library/albums/1/cover")
        );
        assert_eq!(
            q.items()[0].uri,
            "http://192.168.1.5:7788/v1/library/tracks/1/file"
        );
        assert_eq!(q.items()[0].image.as_deref(), Some("/v1/library/albums/7/cover"));
        assert_eq!(
            (&q.items()[1].uri, &q.items()[1].image),
            (&other.uri, &other.image)
        );
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
    fn shuffle_only_touches_upcoming_items() {
        let mut eight = q(8);
        eight.set_current(Some(2));
        let before: Vec<u64> = eight.items().iter().map(|i| i.id).collect();
        let v = eight.version();
        assert!(eight.shuffle_upcoming(0x9e37_79b9));
        assert!(eight.version() > v);
        let after: Vec<u64> = eight.items().iter().map(|i| i.id).collect();
        assert_eq!(&after[..3], &before[..3]);
        assert_eq!(eight.current_index(), Some(2));
        let mut rest = after[3..].to_vec();
        rest.sort_unstable();
        assert_eq!(rest, before[3..]);
        assert_ne!(&after[3..], &before[3..], "8 choose 5 should move something");
        // Nothing current: everything shuffles; one item: nothing to do.
        let mut all = q(5);
        assert!(all.shuffle_upcoming(7));
        let mut one = q(1);
        assert!(!one.shuffle_upcoming(7));
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

    fn ids(q: &Queue) -> Vec<u64> {
        q.items().iter().map(|i| i.id).collect()
    }

    /// Shuffle on keeps the current item and shuffles what comes after; off puts every item
    /// back in its original order, the current item following its id.
    #[test]
    fn shuffle_off_restores_the_order_around_the_current_item() {
        let mut q8 = q(8);
        q8.set_current(Some(2));
        let original = ids(&q8);
        let v = q8.version();
        assert!(q8.set_shuffle(true, 0x9e37_79b9));
        assert!(q8.shuffle());
        assert!(q8.version() > v);
        let on = ids(&q8);
        assert_eq!(&on[..3], &original[..3], "played and current stay");
        assert_ne!(&on[3..], &original[3..]);
        assert!(!q8.set_shuffle(true, 1), "already on: nothing changes");
        // Play on: the current item moves along the play order
        q8.set_current(Some(5));
        let now = q8.current().unwrap().id;
        assert!(q8.set_shuffle(false, 0));
        assert!(!q8.shuffle());
        assert_eq!(ids(&q8), original, "the exact original order");
        assert_eq!(
            q8.current().unwrap().id,
            now,
            "the current item is still the current one"
        );
        assert_eq!(
            q8.current_index(),
            Some(original.iter().position(|i| *i == now).unwrap())
        );
        assert!(!q8.set_shuffle(false, 0));
    }

    /// Items added, removed and moved while shuffled: "Play next" is next in both orders, "Add to
    /// queue" ends both; a removal leaves both; a move changes the play order only.
    #[test]
    fn changes_while_shuffled_keep_both_orders() {
        let mut q5 = q(5);
        q5.set_current(Some(1));
        let original = ids(&q5);
        q5.set_shuffle(true, 42);
        let next = q5.add(vec![QueueItem::new("next")], Insert::Next)[0];
        let end = q5.add(vec![QueueItem::new("end")], Insert::End)[0];
        assert_eq!(q5.items()[2].id, next, "right after the current item");
        assert_eq!(q5.items().last().unwrap().id, end);
        let gone = original[4];
        assert!(q5.remove(gone));
        let last_play = ids(&q5)[q5.len() - 1];
        assert!(q5.move_item(last_play, 2));
        assert!(q5.shuffle(), "a move keeps the mode");
        q5.set_shuffle(false, 0);
        let mut want: Vec<u64> = original.clone();
        want.insert(2, next);
        want.push(end);
        want.retain(|i| *i != gone);
        assert_eq!(ids(&q5), want);
        assert_eq!(q5.current().unwrap().id, original[1]);
    }

    /// A reshuffle puts the current item first and shuffles every other item after it (a queue
    /// loaded with shuffle on); from off it turns shuffle on.
    #[test]
    fn reshuffle_keeps_the_current_item_first() {
        let mut q6 = q(6);
        q6.set_current(Some(3));
        let original = ids(&q6);
        q6.reshuffle(7);
        assert!(q6.shuffle());
        assert_eq!(q6.current_index(), Some(0));
        assert_eq!(q6.current().unwrap().id, original[3]);
        let mut rest = ids(&q6)[1..].to_vec();
        rest.sort_unstable();
        let mut others: Vec<u64> = original.iter().copied().filter(|i| *i != original[3]).collect();
        others.sort_unstable();
        assert_eq!(rest, others, "every other item is there, after it");
        q6.set_shuffle(false, 0);
        assert_eq!(ids(&q6), original);
        assert_eq!(q6.current_index(), Some(3));
        // Nothing current: everything shuffles, nothing is current after
        let mut all = q(4);
        all.reshuffle(9);
        assert_eq!(all.current_index(), None);
        assert_eq!(all.len(), 4);
    }

    /// The original order survives a save (as indexes) and a load with new ids; a bad saved
    /// order makes the play order the original one.
    #[test]
    fn the_original_order_is_saved_as_indexes_and_restored() {
        let mut q5 = q(5);
        q5.set_current(Some(0));
        let original = ids(&q5);
        q5.set_shuffle(true, 3);
        let play: Vec<String> = q5.items().iter().map(|i| i.uri.clone()).collect();
        let order = q5.shuffle_order().unwrap();
        assert_eq!(order.len(), 5);
        // A new queue with the same play order (new ids), as a restart loads it
        let mut again = Queue::default();
        again.add(
            play.iter().map(|u| QueueItem::new(u.clone())).collect(),
            Insert::End,
        );
        again.set_current(Some(0));
        again.restore_shuffle(&order);
        assert!(again.shuffle());
        again.set_shuffle(false, 0);
        let uris: Vec<String> = again.items().iter().map(|i| i.uri.clone()).collect();
        assert_eq!(original.len(), 5);
        let want: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        assert_eq!(uris, want, "the original order, by content");
        assert_eq!(q(3).shuffle_order(), None);
        // A snapshot holds the items and the order from the same look
        let snap = q5.snapshot();
        assert_eq!((snap.shuffle, snap.version), (true, q5.version()));
        assert_eq!(snap.order, q5.shuffle_order().unwrap());
        assert_eq!(snap.items, q5.items());
        let mut bad = q(3);
        bad.restore_shuffle(&[0, 0, 1]);
        assert!(bad.shuffle());
        let before = ids(&bad);
        bad.set_shuffle(false, 0);
        assert_eq!(
            ids(&bad),
            before,
            "a bad saved order: the play order is the original"
        );
    }
}
