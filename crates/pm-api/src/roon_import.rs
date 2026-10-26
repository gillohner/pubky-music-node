//! Moving from Roon: one export at a time (tracks, albums or a playlist, as Roon's Export writes
//! them), matched to this library and reported plainly: what matched, what did not, and why.
//! A dry run first; then what the user chose: Roon favourites into bookmarks, playlists into
//! playlists, and, only when asked, Roon tags as public tags on Pubky.

use crate::state::NodeState;
use crate::types::*;
use crate::{ApiError, ApiResult};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use pm_library::roon::{parse_roon, RoonKind, RoonRow};
use pm_library::{ImportedTrack, Library};
use pm_model::{Album, MusicUri, Track};
use std::collections::BTreeMap;
use std::sync::Arc;

/// At most this many unmatched rows are listed (all are counted).
const LIST_MAX: usize = 300;

fn describe(r: &RoonRow) -> String {
    match (&r.title, &r.album, &r.artist) {
        (Some(t), _, Some(a)) => format!("{t} · {a}"),
        (Some(t), _, None) => t.clone(),
        (None, Some(al), Some(a)) => format!("{al} · {a}"),
        (None, Some(al), None) => al.clone(),
        _ => "?".into(),
    }
}

fn find_track(lib: &Library, r: &RoonRow) -> Result<Option<Track>, ApiError> {
    let Some(title) = r.title.clone() else {
        return Ok(None);
    };
    Ok(lib.resolve_import(&ImportedTrack {
        title,
        artist: r.artist.clone().or_else(|| r.album_artist.clone()),
        album: r.album.clone(),
        path: r.path.clone(),
        duration_ms: r.duration_ms,
    })?)
}

fn find_album(lib: &Library, r: &RoonRow) -> Result<Option<Album>, ApiError> {
    let Some(title) = r.album.as_deref() else {
        return Ok(None);
    };
    let norm = |s: &str| s.trim().to_lowercase();
    let artist = r.album_artist.as_deref().or(r.artist.as_deref()).map(norm);
    Ok(lib.search_albums(title, 25)?.into_iter().find(|a| {
        norm(&a.title) == norm(title)
            && match (&artist, &a.album_artist) {
                (Some(want), Some(have)) => norm(have) == *want,
                _ => true,
            }
    }))
}

/// `POST /v1/import/roon?file=&dry_run=&bookmarks=&tags=&playlist=` with the export as the body
/// (`favourites=` is the deprecated name of `bookmarks=`).
pub(crate) async fn import(
    State(s): State<Arc<NodeState>>,
    Query(q): Query<RoonImportQuery>,
    body: axum::body::Bytes,
) -> ApiResult<RoonImportReport> {
    let export = parse_roon(&body, q.file.as_deref())
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let dry = q.dry_run.unwrap_or(true);
    let mut report = RoonImportReport {
        kind: export.kind,
        name: export.name.clone().or_else(|| {
            q.file
                .as_deref()
                .map(|f| pm_library::import::name_from_file(std::path::Path::new(f)))
        }),
        rows: export.rows.len(),
        dry_run: dry,
        ..Default::default()
    };
    let mut tags: BTreeMap<String, usize> = BTreeMap::new();
    for r in &export.rows {
        for t in &r.tags {
            *tags.entry(t.clone()).or_default() += 1;
        }
    }
    report.tags_found = tags.into_iter().collect();
    report
        .tags_found
        .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    if export.kind == RoonKind::Playlist {
        return playlist(&s, &export, report, &q).await;
    }
    let want_favs = q.bookmarks.unwrap_or(true);
    let want_tags = q.tags.unwrap_or(false);
    {
        let lib = s.library.lock().expect("library poisoned");
        for r in &export.rows {
            // The library object and its canonical identity
            let found: Option<(Option<MusicUri>, String)> = match export.kind {
                RoonKind::Tracks => {
                    find_track(&lib, r)?.map(|t| (t.publishable_uri().cloned(), t.title.clone()))
                }
                _ => find_album(&lib, r)?.map(|a| {
                    (
                        a.release_mbid.map(|mbid| MusicUri::Release { mbid }),
                        a.title.clone(),
                    )
                }),
            };
            let Some((uri, _)) = found else {
                report.unmatched += 1;
                if report.misses.len() < LIST_MAX {
                    report.misses.push(RoonMiss {
                        what: describe(r),
                        reason: if export.kind == RoonKind::Tracks {
                            "not in this library (by file path, file name, then title and artist)".into()
                        } else {
                            "no album with this title and artist in this library".into()
                        },
                    });
                }
                continue;
            };
            report.matched += 1;
            let wants_identity = (want_favs && r.favorite) || (want_tags && !r.tags.is_empty());
            let Some(uri) = uri else {
                if wants_identity {
                    report.not_identified += 1;
                    if report.misses.len() < LIST_MAX {
                        report.misses.push(RoonMiss {
                            what: describe(r),
                            reason: "in the library, but without a MusicBrainz id or ISRC yet: identify it (Settings › Identify library), then import again".into(),
                        });
                    }
                }
                continue;
            };
            if want_favs && r.favorite {
                report.bookmarks += 1;
                report.favourites += 1;
                if !dry {
                    crate::bookmarks::add(&lib, uri.clone())?;
                }
            }
            if want_tags {
                for label in &r.tags {
                    // Pubky labels have no spaces: "late night" becomes "late-night"
                    let label = label
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join("-")
                        .to_lowercase();
                    let Ok((tag, id)) = crate::tags::make_tag(&uri, &label) else {
                        report.tags_skipped += 1;
                        continue;
                    };
                    report.tags_added += 1;
                    if !dry {
                        lib.add_tag(&id, &tag.uri, &tag.label, tag.created_at)?;
                    }
                }
            }
        }
    }
    if !dry {
        s.notify_library();
    }
    Ok(Json(report))
}

async fn playlist(
    s: &Arc<NodeState>,
    export: &pm_library::roon::RoonExport,
    mut report: RoonImportReport,
    q: &RoonImportQuery,
) -> ApiResult<RoonImportReport> {
    {
        let lib = s.library.lock().expect("library poisoned");
        for r in &export.rows {
            match find_track(&lib, r)? {
                Some(t) if t.publishable_uri().is_some() => report.matched += 1,
                Some(_) => {
                    report.matched += 1;
                    report.not_identified += 1;
                    if report.misses.len() < LIST_MAX {
                        report.misses.push(RoonMiss {
                            what: describe(r),
                            reason: "in the library, but without a MusicBrainz id or ISRC yet, so it cannot go into a playlist: identify it, then import again".into(),
                        });
                    }
                }
                None => {
                    report.unmatched += 1;
                    if report.misses.len() < LIST_MAX {
                        report.misses.push(RoonMiss {
                            what: describe(r),
                            reason: "not in this library".into(),
                        });
                    }
                }
            }
        }
    }
    if report.dry_run || !q.playlist.unwrap_or(true) {
        return Ok(Json(report));
    }
    let tracks: Vec<DescribedTrack> = export
        .rows
        .iter()
        .filter_map(|r| {
            Some(DescribedTrack {
                title: r.title.clone()?,
                artist: r.artist.clone(),
                album: r.album.clone(),
                uri: None,
                path: r.path.clone(),
            })
        })
        .collect();
    let name = report.name.clone().unwrap_or_else(|| "From Roon".into());
    match crate::activity::import_tracks(s, name, Some("Moved from Roon".into()), tracks) {
        Ok(p) => report.playlist = Some(p),
        Err(e) if e.status == StatusCode::UNPROCESSABLE_ENTITY => {}
        Err(e) => return Err(e),
    }
    Ok(Json(report))
}
