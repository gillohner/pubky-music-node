//! Offline music for the apps (STEWARD-DOWNLOADS.md; mn-app's part).
//!
//! A phone or laptop keeps songs, albums, playlists and favourites from its node. This crate is
//! everything the app does about that, so the Tauri shell only has to call it:
//! - [`Store`]: the device's folder for one node ([`node_dir`]): the index of what it keeps (the
//!   node's own answers for those songs, playlists and favourites), the song files, covers, the
//!   cached web UI, and the queue of plays and hearts made offline.
//! - [`sync()`]: with the node reachable, send the queue, tell the node what the device keeps and
//!   has (`/v1/devices/{id}/downloads`), download what it still wants (resuming), and drop what
//!   nothing keeps any more.
//! - [`server`]: with the node out of reach, a server on `127.0.0.1` that serves the web UI in
//!   its offline mode from the store.

pub mod server;
pub mod store;
pub mod sync;

pub use store::{node_dir, Event, Format, Index, Keep, KeptTrack, Store};
pub use sync::{flush, sync, Node, SyncError, SyncOptions, SyncReport};
