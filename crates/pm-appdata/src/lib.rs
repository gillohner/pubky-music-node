//! Pubky Music's own data on a person's homeserver: what only this app and its node read and write,
//! under `/priv/pubky.music/` (and `/pub/pubky.music/` where it must be public). The generic music
//! spec, which any app can use, is `pubky-music-specs` (`/{pub,priv}/music/v1/`).
//!
//! Every path is in [`paths`] and in docs/APP-DATA.md, with its owner, format and version. The sources
//! vault is [`vault`] (VAULT.md in this crate).

#![forbid(unsafe_code)]

pub mod paths;
pub mod vault;
