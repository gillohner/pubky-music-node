//! YouTube, official mode (owner, 2026-10-22; STEWARD-DUAL-MODE.md): YouTube and YouTube Music
//! links play in the "This browser" zone through YouTube's own embedded player (IFrame Player
//! API), driven by the page's controls. The audio never leaves YouTube's player, so these items
//! cannot reach the node's zones; a `youtube:video:<id>` queued on a zone fails with that reason.
//! Ads and YouTube's terms apply. The unofficial mode (yt-dlp, for zones) comes later.

use crate::state::NodeState;
use crate::streaming::StreamingService;
use crate::types::*;
use crate::ApiError;

pub const SOURCE_ID: &str = "youtube";
pub const VIDEO_URI_PREFIX: &str = "youtube:video:";
/// Why a zone refuses a YouTube item.
pub const ZONE_REFUSAL: &str = "YouTube plays only in This browser (YouTube's own player)";

pub struct YouTube;

#[async_trait::async_trait]
impl StreamingService for YouTube {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn info(&self, _s: &NodeState) -> SourceInfo {
        SourceInfo {
            id: SOURCE_ID.into(),
            name: "YouTube".into(),
            description: "YouTube and YouTube Music links on song pages play in This browser, in YouTube's own player; its ads and terms apply".into(),
            status: "browser".into(),
        }
    }

    async fn search(&self, _s: &NodeState, _term: &str, _limit: u32) -> Result<Vec<SourceHit>, ApiError> {
        Err(ApiError::bad_request(
            "YouTube search is not available yet: links on song pages play in This browser",
        ))
    }

    async fn item(&self, _s: &NodeState, id: &str) -> Result<SourceItem, ApiError> {
        Err(ApiError::not_found(format!("no YouTube item {id} on this node")))
    }
}
