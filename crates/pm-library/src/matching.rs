//! MusicBrainz ids for library artists and albums whose tags have none: what decides a match.
//!
//! Tags always win. The lookup never matches on a name alone: an album needs a MusicBrainz
//! release with the same title, the same artist credit and the same number of songs (and every
//! release that fits in one release group); an artist needs its library albums to agree on one
//! credited artist of that name, or, without a matched album, its songs to find recordings of
//! the same title and length by one artist of that name. Anything less stays unmatched.
//!
//! These functions take MusicBrainz's search answers as JSON; the node asks the questions
//! ([`album_query`], [`recording_query`]) through its shared MusicBrainz gate.

use crate::db::{now_unix, Library};
use crate::error::Result;
use pm_model::Mbid;
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::collections::HashMap;

/// A song's length on MusicBrainz may differ from the file's by this much.
pub const LENGTH_SLACK_MS: u64 = 3000;

/// A library album, as the lookup sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumEvidence {
    pub id: i64,
    pub title: String,
    /// The album artist, else the first song's artist.
    pub artist: String,
    /// Songs the library has on it.
    pub tracks: u32,
}

/// A library song, for an artist without a matched album.
#[derive(Debug, Clone, PartialEq)]
pub struct SongEvidence {
    pub title: String,
    pub duration_ms: u64,
}

/// What the lookup decided for an album.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AlbumDecision {
    pub release_group: Option<Mbid>,
    /// The fitting release's credited artists, for the artist decision.
    pub credits: Vec<(Mbid, String)>,
    /// Why, in a few words (kept with the match).
    pub evidence: String,
}

/// A name as people compare them: lower case, `&` as "and", punctuation as spaces, no leading
/// "the".
pub fn plain_name(s: &str) -> String {
    let s = s.to_lowercase().replace('&', " and ");
    let words: Vec<String> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let words = match words.first().map(String::as_str) {
        Some("the") if words.len() > 1 => &words[1..],
        _ => &words[..],
    };
    words.join(" ")
}

/// A title as people compare them: [`plain_name`] without "(Deluxe Edition)", "\[Remastered\]"
/// or " - 2011 Remaster". A title that is nothing but brackets keeps them.
pub fn plain_title(s: &str) -> String {
    plain_name(&bare_title(s))
}

/// A title without its edition: no "(Deluxe Edition)", "\[Remastered\]" or " - 2011 Remaster",
/// as typed otherwise. What a search asks for: MusicBrainz's titles carry no such suffix. A title
/// that is nothing but brackets is kept.
pub fn bare_title(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    let head = s.split(" - ").next().unwrap_or(s);
    for c in head.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let bare = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if plain_name(&bare).is_empty() {
        s.trim().to_string()
    } else {
        bare
    }
}

/// The release search for an album: title and artist, as a Lucene query.
pub fn album_query(a: &AlbumEvidence) -> String {
    format!(
        "release:\"{}\" AND artist:\"{}\"",
        lucene_escape(&bare_title(&a.title)),
        lucene_escape(&a.artist)
    )
}

/// The recording search for a song by `artist`, its length within [`LENGTH_SLACK_MS`].
pub fn recording_query(artist: &str, s: &SongEvidence) -> String {
    format!(
        "recording:\"{}\" AND artist:\"{}\" AND dur:[{} TO {}]",
        lucene_escape(&bare_title(&s.title)),
        lucene_escape(artist),
        s.duration_ms.saturating_sub(LENGTH_SLACK_MS),
        s.duration_ms + LENGTH_SLACK_MS
    )
}

/// The credit as one line ("A feat. B") and each artist in it.
fn credit_fits(credit: &[(Option<Mbid>, String, String)], artist: &str) -> bool {
    let want = plain_name(artist);
    if want.is_empty() {
        return false;
    }
    let line: String = credit
        .iter()
        .map(|(_, name, join)| format!("{name}{join}"))
        .collect();
    plain_name(&line) == want || credit.iter().any(|(_, name, _)| plain_name(name) == want)
}

/// Decide an album from a `release` search answer.
pub fn decide_album(a: &AlbumEvidence, answer: &Value) -> AlbumDecision {
    let title = plain_title(&a.title);
    let mut fits: Vec<(Mbid, Vec<(Mbid, String)>)> = Vec::new();
    for r in answer["releases"].as_array().into_iter().flatten() {
        let count = r["track-count"]
            .as_u64()
            .or_else(|| {
                r["media"]
                    .as_array()
                    .map(|m| m.iter().filter_map(|x| x["track-count"].as_u64()).sum())
            })
            .unwrap_or(0);
        let credit = credits_of(r);
        let Some(group) = mbid_at(&r["release-group"], "id") else {
            continue;
        };
        if count == a.tracks as u64
            && plain_title(r["title"].as_str().unwrap_or_default()) == title
            && credit_fits(&credit, &a.artist)
        {
            let ids = credit
                .into_iter()
                .filter_map(|(id, name, _)| Some((id?, name)))
                .collect();
            fits.push((group, ids));
        }
    }
    let mut groups: Vec<Mbid> = fits.iter().map(|(g, _)| *g).collect();
    groups.sort_by_key(|g| g.to_string());
    groups.dedup();
    match groups.len() {
        0 => AlbumDecision {
            evidence: format!("no release of {} songs by {} with this title", a.tracks, a.artist),
            ..AlbumDecision::default()
        },
        1 => {
            let credits = fits.swap_remove(0).1;
            AlbumDecision {
                release_group: Some(groups[0]),
                credits,
                evidence: format!("a release of {} songs, title and artist", a.tracks),
            }
        }
        n => AlbumDecision {
            evidence: format!("releases that fit are in {n} release groups"),
            ..AlbumDecision::default()
        },
    }
}

/// Artists of `name` credited on a recording search answer's hits (one set per answer).
pub fn recording_artists(name: &str, s: &SongEvidence, answer: &Value) -> Vec<Mbid> {
    let want = plain_name(name);
    let title = plain_title(&s.title);
    let mut out: Vec<Mbid> = Vec::new();
    for r in answer["recordings"].as_array().into_iter().flatten() {
        let close = r["length"]
            .as_u64()
            .is_some_and(|l| l.abs_diff(s.duration_ms) <= LENGTH_SLACK_MS);
        if !close || plain_title(r["title"].as_str().unwrap_or_default()) != title {
            continue;
        }
        for (id, credited, _) in credits_of(r) {
            if let Some(id) = id.filter(|_| plain_name(&credited) == want) {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// Decide an artist: from its matched albums' credits, else its songs' recording hits. Every
/// piece of evidence must name the same one artist of that name.
pub fn decide_artist(
    name: &str,
    album_credits: &[Vec<(Mbid, String)>],
    song_artists: &[Vec<Mbid>],
) -> (Option<Mbid>, String) {
    let want = plain_name(name);
    let mut from_albums: Vec<Mbid> = Vec::new();
    let mut albums = 0;
    for credits in album_credits {
        let mut here = false;
        for (id, credited) in credits {
            if plain_name(credited) == want {
                here = true;
                if !from_albums.contains(id) {
                    from_albums.push(*id);
                }
            }
        }
        albums += here as usize;
    }
    match from_albums.len() {
        1 => {
            return (
                Some(from_albums[0]),
                format!("credited on {}", plural(albums, "matched album")),
            )
        }
        0 => {}
        _ => {
            return (
                None,
                "matched albums credit different artists of this name".into(),
            )
        }
    }
    let found: Vec<&Vec<Mbid>> = song_artists.iter().filter(|s| !s.is_empty()).collect();
    if found.is_empty() {
        return (None, "no matched album and no song found".into());
    }
    let mut ids: Vec<Mbid> = found.iter().flat_map(|s| s.iter().copied()).collect();
    ids.sort_by_key(|g| g.to_string());
    ids.dedup();
    if ids.len() == 1 {
        (
            Some(ids[0]),
            format!("{} by this artist, same length", plural(found.len(), "song")),
        )
    } else {
        (None, "songs found by different artists of this name".into())
    }
}

/// An unsure answer is asked again after this long, or sooner when the library's evidence grows.
pub const RETRY_UNMATCHED_SECS: i64 = 30 * 24 * 3600;

/// Where an id came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchSource {
    /// The files' tags, or MusicBrainz asked by a tagged id.
    Tags,
    /// Looked up by title, artist and song count.
    Lookup,
}

/// An artist's or album's MusicBrainz id (an album's release group) and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MbMatch {
    pub mbid: Mbid,
    pub source: MatchSource,
}

/// A decision to keep: `mbid` None means not sure.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchRow {
    pub mbid: Option<Mbid>,
    pub source: MatchSource,
    pub evidence: String,
    pub credits: Vec<(Mbid, String)>,
    /// An album's songs, an artist's albums, when it was decided.
    pub size: i64,
    pub checked_at: i64,
}

/// How to find an album's release group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumAsk {
    /// Tagged: ask for its credits (an artist on it has no id in the tags).
    ReleaseGroup(Mbid),
    /// A tagged release without its release group.
    Release(Mbid),
    /// No id: search by title, artist and song count.
    Search,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlbumTask {
    pub key: String,
    pub album: AlbumEvidence,
    pub ask: AlbumAsk,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtistTask {
    pub id: i64,
    pub key: String,
    pub name: String,
    /// Its albums' keys; the artist is decided once each has an answer.
    pub albums: Vec<String>,
}

/// What still needs asking.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MatchPlan {
    pub albums: Vec<AlbumTask>,
    /// Artists whose albums are all answered.
    pub artists: Vec<ArtistTask>,
}

/// How many have an id, by where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct KindCounts {
    pub total: u64,
    pub tags: u64,
    pub lookup: u64,
    pub unmatched: u64,
    pub pending: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MatchCounts {
    pub artists: KindCounts,
    pub albums: KindCounts,
}

/// The key an album's answer is kept under: two editions with different songs are two albums.
pub fn album_key(title: &str, artist: &str, tracks: i64) -> String {
    format!("{}\u{1f}{}\u{1f}{tracks}", plain_title(title), plain_name(artist))
}

fn fresh(row: Option<&MatchRow>, size: i64, now: i64) -> bool {
    row.is_some_and(|r| r.mbid.is_some() || (r.size == size && now - r.checked_at < RETRY_UNMATCHED_SECS))
}

/// A library album as the matcher reads it.
struct AlbumRow {
    id: i64,
    title: String,
    artist: String,
    tracks: i64,
    release_group: Option<Mbid>,
    release: Option<Mbid>,
    untagged_artist: bool,
}

/// A library artist as the matcher reads it.
struct ArtistRow {
    id: i64,
    name: String,
    mbid: Option<Mbid>,
    albums: Vec<i64>,
}

impl Library {
    fn match_rows(&self, kind: &str) -> Result<HashMap<String, MatchRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT key, mbid, source, evidence, credits, size, checked_at FROM mb_matches WHERE kind = ?1",
        )?;
        let rows = stmt.query_map(params![kind], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = HashMap::new();
        for r in rows {
            let (key, mbid, source, evidence, credits, size, checked_at) = r?;
            out.insert(key, row_of(mbid, &source, evidence, &credits, size, checked_at));
        }
        Ok(out)
    }

    fn match_row(&self, kind: &str, key: &str) -> Result<Option<MatchRow>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT mbid, source, evidence, credits, size, checked_at FROM mb_matches WHERE kind = ?1 AND key = ?2",
                params![kind, key],
                |r| {
                    Ok(row_of(
                        r.get(0)?,
                        &r.get::<_, String>(1)?,
                        r.get(2)?,
                        &r.get::<_, String>(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?)
    }

    /// Keep a decision (`kind` is `artist` or `album`).
    pub fn save_match(&self, kind: &str, key: &str, row: &MatchRow) -> Result<()> {
        let credits: Vec<(String, &str)> = row
            .credits
            .iter()
            .map(|(m, n)| (m.to_string(), n.as_str()))
            .collect();
        self.lock().execute(
            "INSERT INTO mb_matches(kind, key, mbid, source, evidence, credits, size, checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(kind, key) DO UPDATE SET mbid = excluded.mbid, source = excluded.source,
             evidence = excluded.evidence, credits = excluded.credits, size = excluded.size,
             checked_at = excluded.checked_at",
            params![
                kind,
                key,
                row.mbid.map(|m| m.to_string()),
                source_name(row.source),
                row.evidence,
                serde_json::to_string(&credits).unwrap_or_else(|_| "[]".into()),
                row.size,
                row.checked_at
            ],
        )?;
        Ok(())
    }

    /// Every album, or just `only`.
    fn albums_for_matching(&self, only: Option<i64>) -> Result<Vec<AlbumRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT al.id, al.title,
                    COALESCE(al.album_artist, (SELECT a.name FROM tracks t
                        JOIN track_artists ta ON ta.track_id = t.id JOIN artists a ON a.id = ta.artist_id
                        WHERE t.album_id = al.id ORDER BY t.disc_no, t.track_no, ta.position LIMIT 1), ''),
                    (SELECT COUNT(*) FROM tracks t WHERE t.album_id = al.id),
                    al.release_group_mbid, al.release_mbid,
                    EXISTS (SELECT 1 FROM tracks t JOIN track_artists ta ON ta.track_id = t.id
                        JOIN artists a ON a.id = ta.artist_id WHERE t.album_id = al.id AND a.mbid IS NULL)
             FROM albums al WHERE ?1 IS NULL OR al.id = ?1 ORDER BY al.id",
        )?;
        let rows = stmt.query_map(params![only], |r| {
            Ok(AlbumRow {
                id: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                tracks: r.get(3)?,
                release_group: r.get::<_, Option<String>>(4)?.and_then(|m| Mbid::parse(&m).ok()),
                release: r.get::<_, Option<String>>(5)?.and_then(|m| Mbid::parse(&m).ok()),
                untagged_artist: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Artists without an id in the tags, one per plain name (artists whose names differ only
    /// in case or "The" are one answer): the first row's id and name, and all their albums.
    fn untagged_artists(&self) -> Result<Vec<ArtistRow>> {
        let mut out: Vec<ArtistRow> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for a in self.artists_for_matching()? {
            let key = plain_name(&a.name);
            if a.mbid.is_some() || key.is_empty() || a.id == crate::UNKNOWN_ARTIST_ID {
                continue;
            }
            match at.get(&key) {
                Some(&i) => {
                    for al in a.albums {
                        if !out[i].albums.contains(&al) {
                            out[i].albums.push(al);
                        }
                    }
                }
                None => {
                    at.insert(key, out.len());
                    out.push(a);
                }
            }
        }
        Ok(out)
    }

    fn artists_for_matching(&self) -> Result<Vec<ArtistRow>> {
        let conn = self.lock();
        let mut albums: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT ta.artist_id, t.album_id FROM track_artists ta JOIN tracks t ON t.id = ta.track_id
             WHERE t.album_id IS NOT NULL",
        )?;
        for r in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (artist, album) = r?;
            albums.entry(artist).or_default().push(album);
        }
        let mut stmt = conn.prepare_cached(
            "SELECT a.id, a.name, a.mbid FROM artists a
             WHERE EXISTS (SELECT 1 FROM track_artists ta WHERE ta.artist_id = a.id) ORDER BY a.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, name, mbid) = r?;
            out.push(ArtistRow {
                id,
                name,
                mbid: mbid.and_then(|m| Mbid::parse(&m).ok()),
                albums: albums.remove(&id).unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// What the matcher still has to ask, albums first; artists only once their albums are
    /// answered.
    pub fn match_plan(&self, now: i64) -> Result<MatchPlan> {
        let album_rows = self.match_rows("album")?;
        let artist_rows = self.match_rows("artist")?;
        let albums = self.albums_for_matching(None)?;
        let mut key_of: HashMap<i64, String> = HashMap::new();
        let mut plan = MatchPlan::default();
        let mut pending: std::collections::HashSet<String> = Default::default();
        for a in &albums {
            let key = album_key(&a.title, &a.artist, a.tracks);
            key_of.insert(a.id, key.clone());
            let ask = match (a.release_group, a.release) {
                (Some(_), _) if !a.untagged_artist => continue,
                (Some(g), _) => AlbumAsk::ReleaseGroup(g),
                (None, Some(r)) => AlbumAsk::Release(r),
                (None, None) if a.artist.trim().is_empty() || a.tracks == 0 => continue,
                (None, None) => AlbumAsk::Search,
            };
            if fresh(album_rows.get(&key), a.tracks, now) || !pending.insert(key.clone()) {
                continue;
            }
            plan.albums.push(AlbumTask {
                key,
                album: AlbumEvidence {
                    id: a.id,
                    title: a.title.clone(),
                    artist: a.artist.clone(),
                    tracks: a.tracks as u32,
                },
                ask,
            });
        }
        for a in self.untagged_artists()? {
            let key = plain_name(&a.name);
            if fresh(artist_rows.get(&key), a.albums.len() as i64, now) {
                continue;
            }
            let keys: Vec<String> = a.albums.iter().filter_map(|id| key_of.get(id).cloned()).collect();
            if keys.iter().any(|k| pending.contains(k)) {
                continue;
            }
            plan.artists.push(ArtistTask {
                id: a.id,
                key,
                name: a.name,
                albums: keys,
            });
        }
        Ok(plan)
    }

    /// The credits kept with these albums' answers (tagged release groups' too).
    pub fn album_credits(&self, keys: &[String]) -> Result<Vec<Vec<(Mbid, String)>>> {
        let mut out = Vec::new();
        for k in keys {
            if let Some(r) = self.match_row("album", k)? {
                out.push(r.credits);
            }
        }
        Ok(out)
    }

    /// An artist's MusicBrainz id: from the tags, else a lookup's.
    pub fn artist_match(&self, artist_id: i64) -> Result<Option<MbMatch>> {
        let row: Option<(String, Option<String>)> = self
            .lock()
            .query_row(
                "SELECT name, mbid FROM artists WHERE id = ?1",
                params![artist_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((name, mbid)) = row else {
            return Ok(None);
        };
        if let Some(m) = mbid.and_then(|m| Mbid::parse(&m).ok()) {
            return Ok(Some(MbMatch {
                mbid: m,
                source: MatchSource::Tags,
            }));
        }
        Ok(self.match_row("artist", &plain_name(&name))?.and_then(|r| {
            Some(MbMatch {
                mbid: r.mbid?,
                source: r.source,
            })
        }))
    }

    /// An album's release group: from the tags, else asked by its tagged release, else a
    /// lookup's.
    pub fn album_match(&self, album_id: i64) -> Result<Option<MbMatch>> {
        let Some(a) = self.albums_for_matching(Some(album_id))?.into_iter().next() else {
            return Ok(None);
        };
        if let Some(m) = a.release_group {
            return Ok(Some(MbMatch {
                mbid: m,
                source: MatchSource::Tags,
            }));
        }
        Ok(self
            .match_row("album", &album_key(&a.title, &a.artist, a.tracks))?
            .and_then(|r| {
                Some(MbMatch {
                    mbid: r.mbid?,
                    source: r.source,
                })
            }))
    }

    /// How much of the library has ids, and from where.
    pub fn match_counts(&self) -> Result<MatchCounts> {
        let now = now_unix();
        let album_rows = self.match_rows("album")?;
        let artist_rows = self.match_rows("artist")?;
        let mut c = MatchCounts::default();
        for a in self.albums_for_matching(None)? {
            let k = &mut c.albums;
            k.total += 1;
            let row = album_rows.get(&album_key(&a.title, &a.artist, a.tracks));
            match (a.release_group, row) {
                (Some(_), _) => k.tags += 1,
                (None, Some(r)) if r.mbid.is_some() => match r.source {
                    MatchSource::Tags => k.tags += 1,
                    MatchSource::Lookup => k.lookup += 1,
                },
                (None, Some(r)) if fresh(Some(r), a.tracks, now) => k.unmatched += 1,
                _ if a.artist.trim().is_empty() || a.tracks == 0 => k.unmatched += 1,
                _ => k.pending += 1,
            }
        }
        let groups: HashMap<String, i64> = self
            .untagged_artists()?
            .into_iter()
            .map(|a| (plain_name(&a.name), a.albums.len() as i64))
            .collect();
        for a in self.artists_for_matching()? {
            let k = &mut c.artists;
            k.total += 1;
            let key = plain_name(&a.name);
            let row = artist_rows.get(&key);
            let size = groups.get(&key).copied().unwrap_or(a.albums.len() as i64);
            match (a.mbid, row) {
                (Some(_), _) => k.tags += 1,
                (None, Some(r)) if r.mbid.is_some() => k.lookup += 1,
                (None, Some(r)) if fresh(Some(r), size, now) => k.unmatched += 1,
                _ if key.is_empty() => k.unmatched += 1,
                _ => k.pending += 1,
            }
        }
        Ok(c)
    }
}

fn source_name(s: MatchSource) -> &'static str {
    match s {
        MatchSource::Tags => "tags",
        MatchSource::Lookup => "lookup",
    }
}

fn row_of(
    mbid: Option<String>,
    source: &str,
    evidence: String,
    credits: &str,
    size: i64,
    checked_at: i64,
) -> MatchRow {
    let credits: Vec<(String, String)> = serde_json::from_str(credits).unwrap_or_default();
    MatchRow {
        mbid: mbid.and_then(|m| Mbid::parse(&m).ok()),
        source: if source == "tags" {
            MatchSource::Tags
        } else {
            MatchSource::Lookup
        },
        evidence,
        credits: credits
            .into_iter()
            .filter_map(|(m, n)| Some((Mbid::parse(&m).ok()?, n)))
            .collect(),
        size,
        checked_at,
    }
}

/// The release group and credits of a release (`inc=release-groups+artist-credits`) or a
/// release group (`inc=artist-credits`, `group` is the one asked) looked up by id.
pub fn looked_up(answer: &Value, group: Option<Mbid>) -> (Option<Mbid>, Vec<(Mbid, String)>) {
    let group = group.or_else(|| mbid_at(&answer["release-group"], "id"));
    let credits = credits_of(answer)
        .into_iter()
        .filter_map(|(id, name, _)| Some((id?, name)))
        .collect();
    (group, credits)
}

/// Escape Lucene special characters inside a quoted phrase.
pub fn lucene_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn mbid_at(v: &Value, key: &str) -> Option<Mbid> {
    v[key].as_str().and_then(|s| Mbid::parse(s).ok())
}

/// An `artist-credit`: each artist's id, the name as credited, and the join phrase after it.
fn credits_of(v: &Value) -> Vec<(Option<Mbid>, String, String)> {
    v["artist-credit"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            (
                mbid_at(&c["artist"], "id"),
                c["name"]
                    .as_str()
                    .or(c["artist"]["name"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                c["joinphrase"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RG1: &str = "0b5d3e2b-4b0c-4d5f-9d4b-1e9a2b3c4d5e";
    const RG2: &str = "1c6e4f3c-5c1d-4e6a-8e5c-2f0b3c4d5e6f";
    const A1: &str = "2d7f5a4d-6d2e-4f7b-9f6d-3a1c4d5e6f70";
    const A2: &str = "3e8a6b5e-7e3f-4a8c-8a7e-4b2d5e6f7081";

    fn release(title: &str, tracks: u64, rg: &str, credit: Value) -> Value {
        json!({"title": title, "track-count": tracks, "release-group": {"id": rg}, "artist-credit": credit})
    }

    fn by(name: &str, id: &str) -> Value {
        json!([{"name": name, "joinphrase": "", "artist": {"id": id, "name": name}}])
    }

    fn album(title: &str, artist: &str, tracks: u32) -> AlbumEvidence {
        AlbumEvidence {
            id: 1,
            title: title.into(),
            artist: artist.into(),
            tracks,
        }
    }

    #[test]
    fn names_and_titles_compare_plainly() {
        assert_eq!(plain_name("The Beatles"), "beatles");
        assert_eq!(plain_name("Simon & Garfunkel"), plain_name("Simon and Garfunkel"));
        assert_eq!(plain_name("AC/DC"), "ac dc");
        assert_eq!(plain_name("The The"), "the");
        assert_eq!(plain_title("OK Computer (Deluxe Edition)"), "ok computer");
        assert_eq!(plain_title("Abbey Road - 2019 Remix"), "abbey road");
        assert_eq!(plain_title("[Untitled]"), "untitled");
        assert_eq!(plain_title("Björk Live"), "björk live");
        assert_eq!(bare_title("Zahltag (Deluxe Edition)"), "Zahltag");
        assert_eq!(bare_title("Abbey Road - 2019 Remix"), "Abbey Road");
        assert_eq!(bare_title("Who  Made Who [Remastered]"), "Who Made Who");
        assert_eq!(bare_title("[Untitled]"), "[Untitled]");
    }

    #[test]
    fn an_album_matches_on_title_artist_and_song_count() {
        let a = album("Zahltag (Deluxe)", "7 Dollar Taxi", 12);
        let answer = json!({"releases": [
            release("Zahltag", 11, RG2, by("7 Dollar Taxi", A1)),
            release("Zahltag", 12, RG1, by("7 Dollar Taxi", A1)),
            release("Zahltag", 12, RG1, by("7 Dollar Taxi", A1)),
            release("Zahltag", 12, RG2, by("Someone Else", A2)),
        ]});
        let d = decide_album(&a, &answer);
        assert_eq!(d.release_group, Some(Mbid::parse(RG1).unwrap()));
        assert_eq!(
            d.credits,
            vec![(Mbid::parse(A1).unwrap(), "7 Dollar Taxi".to_string())]
        );
    }

    #[test]
    fn an_album_with_another_song_count_or_two_groups_stays_unmatched() {
        let a = album("Greatest Hits", "Queen", 14);
        let fewer = json!({"releases": [release("Greatest Hits", 17, RG1, by("Queen", A1))]});
        let d = decide_album(&a, &fewer);
        assert_eq!(d.release_group, None);
        assert!(d.evidence.contains("no release of 14 songs"), "{}", d.evidence);
        let two = json!({"releases": [
            release("Greatest Hits", 14, RG1, by("Queen", A1)),
            release("Greatest Hits", 14, RG2, by("Queen", A1)),
        ]});
        let d = decide_album(&a, &two);
        assert_eq!(d.release_group, None);
        assert!(d.evidence.contains("2 release groups"), "{}", d.evidence);
    }

    #[test]
    fn an_album_credited_to_several_artists_fits_either_way() {
        let credit = json!([
            {"name": "Simon", "joinphrase": " & ", "artist": {"id": A1, "name": "Simon"}},
            {"name": "Garfunkel", "joinphrase": "", "artist": {"id": A2, "name": "Garfunkel"}}
        ]);
        let answer = json!({"releases": [release("Bookends", 12, RG1, credit)]});
        for artist in ["Simon and Garfunkel", "Simon"] {
            let d = decide_album(&album("Bookends", artist, 12), &answer);
            assert_eq!(d.release_group, Some(Mbid::parse(RG1).unwrap()), "{artist}");
        }
        assert_eq!(
            decide_album(&album("Bookends", "Art", 12), &answer).release_group,
            None
        );
    }

    #[test]
    fn a_song_count_from_the_media_counts() {
        let r = json!({"title": "Double", "media": [{"track-count": 10}, {"track-count": 8}],
            "release-group": {"id": RG1}, "artist-credit": by("X", A1)});
        let d = decide_album(&album("Double", "X", 18), &json!({"releases": [r]}));
        assert_eq!(d.release_group, Some(Mbid::parse(RG1).unwrap()));
    }

    #[test]
    fn an_artist_takes_the_id_its_albums_agree_on() {
        let a1 = Mbid::parse(A1).unwrap();
        let a2 = Mbid::parse(A2).unwrap();
        let credits = vec![
            vec![(a1, "7 Dollar Taxi".to_string())],
            vec![(a1, "7 Dollar Taxi".to_string()), (a2, "Guest".to_string())],
        ];
        let (m, why) = decide_artist("7 Dollar Taxi", &credits, &[]);
        assert_eq!(m, Some(a1));
        assert!(why.contains("2 matched albums"), "{why}");
        // Two artists of the same name on different albums: not sure
        let split = vec![
            vec![(a1, "Nirvana".to_string())],
            vec![(a2, "Nirvana".to_string())],
        ];
        assert_eq!(decide_artist("Nirvana", &split, &[]).0, None);
    }

    #[test]
    fn a_name_alone_never_matches() {
        // No album and no song: nothing to go on
        assert_eq!(decide_artist("John Smith", &[], &[]).0, None);
        assert_eq!(decide_artist("John Smith", &[], &[vec![], vec![]]).0, None);
        // An album credited to someone else says nothing about this name
        let other = vec![vec![(Mbid::parse(A2).unwrap(), "Someone".to_string())]];
        assert_eq!(decide_artist("John Smith", &other, &[]).0, None);
    }

    #[test]
    fn songs_decide_an_artist_without_a_matched_album() {
        let a1 = Mbid::parse(A1).unwrap();
        let a2 = Mbid::parse(A2).unwrap();
        let song = SongEvidence {
            title: "Taxi".into(),
            duration_ms: 200_000,
        };
        let answer = json!({"recordings": [
            {"title": "Taxi", "length": 201_500, "artist-credit": by("7 Dollar Taxi", A1)},
            {"title": "Taxi (Live)", "length": 260_000, "artist-credit": by("7 Dollar Taxi", A2)},
            {"title": "Taxi", "length": 230_000, "artist-credit": by("7 Dollar Taxi", A2)},
            {"title": "Taxi", "length": 200_000, "artist-credit": by("Someone", A2)},
        ]});
        let found = recording_artists("7 Dollar Taxi", &song, &answer);
        assert_eq!(found, vec![a1]);
        assert_eq!(
            decide_artist("7 Dollar Taxi", &[], &[found.clone(), vec![]]).0,
            Some(a1)
        );
        // Two songs pointing at two artists of that name: not sure
        assert_eq!(decide_artist("7 Dollar Taxi", &[], &[found, vec![a2]]).0, None);
    }

    #[test]
    fn queries_escape_and_bound_the_length() {
        let q = recording_query(
            "AC/DC",
            &SongEvidence {
                title: "T.N.T.".into(),
                duration_ms: 1000,
            },
        );
        assert_eq!(
            q,
            "recording:\"T.N.T.\" AND artist:\"AC\\/DC\" AND dur:[0 TO 4000]"
        );
        assert_eq!(
            album_query(&album("Who Made Who? (Deluxe)", "AC/DC", 9)),
            "release:\"Who Made Who\\?\" AND artist:\"AC\\/DC\"",
            "the search asks for the title without its edition"
        );
    }

    fn track(file: &str, title: &str, artist: &str, album: &str) -> pm_model::Track {
        pm_model::Track {
            id: None,
            title: title.into(),
            artists: vec![pm_model::Artist {
                id: None,
                name: artist.into(),
                mbid: None,
            }],
            album: Some(pm_model::Album {
                id: None,
                title: album.into(),
                album_artist: Some(artist.into()),
                release_mbid: None,
                release_group_mbid: None,
                year: None,
                upc: None,
                total_discs: None,
            }),
            track_no: Some(1),
            disc_no: Some(1),
            duration_ms: Some(200_000),
            file: pm_model::AudioFile {
                path: file.into(),
                size_bytes: 1,
                mtime_unix: 0,
                container: "flac".into(),
                format: pm_model::AudioFormat::pcm(44_100, 16, 2),
                duration_ms: Some(200_000),
            },
            recording_mbid: None,
            isrc: None,
            acoustid: None,
            canonical: None,
            replaygain_track_gain_db: None,
            replaygain_album_gain_db: None,
            genres: vec![],
        }
    }

    fn unsure(size: i64, checked_at: i64) -> MatchRow {
        MatchRow {
            mbid: None,
            source: MatchSource::Lookup,
            evidence: String::new(),
            credits: Vec::new(),
            size,
            checked_at,
        }
    }

    #[test]
    fn unsure_answers_are_asked_again_when_the_library_has_more_or_a_month_on() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/m/1.flac", "One", "Band", "First"))
            .unwrap();
        let now = 1_000_000_000;
        let plan = lib.match_plan(now).unwrap();
        assert_eq!(plan.albums.len(), 1);
        assert!(plan.artists.is_empty(), "the artist waits for its album");
        lib.save_match("album", &plan.albums[0].key, &unsure(1, now))
            .unwrap();
        let plan = lib.match_plan(now).unwrap();
        assert!(plan.albums.is_empty());
        assert_eq!(plan.artists.len(), 1);
        lib.save_match("artist", "band", &unsure(1, now)).unwrap();
        assert_eq!(lib.match_plan(now).unwrap(), MatchPlan::default());
        // A month on, both are asked again
        let later = lib.match_plan(now + RETRY_UNMATCHED_SECS).unwrap();
        assert_eq!(later.albums.len(), 1);
        // A second album: the artist is asked again once that album is answered
        lib.upsert_track(&track("/m/2.flac", "Two", "Band", "Second"))
            .unwrap();
        let plan = lib.match_plan(now).unwrap();
        assert_eq!(plan.albums.len(), 1);
        lib.save_match("album", &plan.albums[0].key, &unsure(1, now))
            .unwrap();
        assert_eq!(lib.match_plan(now).unwrap().artists.len(), 1);
    }

    #[test]
    fn a_match_survives_a_rescan_and_tags_win() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/m/1.flac", "One", "Band", "First"))
            .unwrap();
        let found = Mbid::parse(A1).unwrap();
        lib.save_match(
            "artist",
            "band",
            &MatchRow {
                mbid: Some(found),
                ..unsure(1, 0)
            },
        )
        .unwrap();
        let id = lib.list_artists().unwrap()[0].id;
        assert_eq!(
            lib.artist_match(id).unwrap(),
            Some(MbMatch {
                mbid: found,
                source: MatchSource::Lookup
            })
        );
        // The file goes and comes back (a new artist row): the answer is kept by name
        lib.remove_folder(std::path::Path::new("/m")).unwrap();
        assert!(lib.list_artists().unwrap().is_empty());
        lib.upsert_track(&track("/m/1.flac", "One", "The Band", "First"))
            .unwrap();
        let id = lib.list_artists().unwrap()[0].id;
        assert_eq!(lib.artist_match(id).unwrap().map(|m| m.mbid), Some(found));
        // An id in the tags wins over the lookup
        let mut tagged = track("/m/3.flac", "Three", "Band", "Third");
        tagged.artists[0].mbid = Some(Mbid::parse(A2).unwrap());
        lib.upsert_track(&tagged).unwrap();
        let tagged_id = lib
            .list_artists()
            .unwrap()
            .into_iter()
            .find(|a| a.mbid.is_some())
            .unwrap()
            .id;
        assert_eq!(
            lib.artist_match(tagged_id).unwrap(),
            Some(MbMatch {
                mbid: Mbid::parse(A2).unwrap(),
                source: MatchSource::Tags
            })
        );
    }

    #[test]
    fn two_editions_and_two_spellings_are_asked_once_each_pass() {
        let lib = Library::open_in_memory().unwrap();
        lib.upsert_track(&track("/m/1.flac", "One", "The Band", "Hits"))
            .unwrap();
        // The deluxe edition: two songs, its album artist spelled "band"
        for (file, title) in [("/m/2.flac", "One"), ("/m/3.flac", "Two")] {
            let mut deluxe = track(file, title, "Band", "Hits");
            deluxe.album.as_mut().unwrap().album_artist = Some("band".into());
            lib.upsert_track(&deluxe).unwrap();
        }
        let now = now_unix();
        let plan = lib.match_plan(now).unwrap();
        // "Hits" with one song and "Hits" with two are two questions
        assert_eq!(plan.albums.len(), 2, "{plan:?}");
        for t in &plan.albums {
            lib.save_match("album", &t.key, &unsure(t.album.tracks as i64, now))
                .unwrap();
        }
        let plan = lib.match_plan(now).unwrap();
        assert!(plan.albums.is_empty(), "{plan:?}");
        // "The Band" and "Band" are one artist question, with both albums
        assert_eq!(plan.artists.len(), 1, "{plan:?}");
        let t = &plan.artists[0];
        assert_eq!(t.albums.len(), 2);
        lib.save_match("artist", &t.key, &unsure(t.albums.len() as i64, now))
            .unwrap();
        assert_eq!(lib.match_plan(now).unwrap(), MatchPlan::default());
        let c = lib.match_counts().unwrap();
        assert_eq!(
            (c.albums.unmatched, c.artists.unmatched, c.artists.pending),
            (2, 2, 0),
            "{c:?}"
        );
    }
}
