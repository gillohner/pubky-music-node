"""The node's library in Home Assistant's media browser, and its search."""

from __future__ import annotations

from typing import Any

from homeassistant.components import media_source
from homeassistant.components.media_player import BrowseError, BrowseMedia, MediaClass, MediaType
from homeassistant.core import HomeAssistant

from .api import PubkyMusicApi
from .const import URI_SCHEME

LIMIT = 500

# Folders under the root: id, title, what they hold
LIBRARY = [
    ("home", "Home", MediaClass.DIRECTORY),
    ("albums", "Albums", MediaClass.ALBUM),
    ("artists", "Artists", MediaClass.ARTIST),
    ("playlists", "Playlists", MediaClass.PLAYLIST),
    ("genres", "Genres", MediaClass.GENRE),
]
HOME_ROWS = [
    ("favourite_albums", "Favourite albums"),
    ("recently_added", "Recently added"),
    ("recently_played", "Recently played"),
    ("most_played_albums", "Most played"),
]


def uri(kind: str, ident: Any = "") -> str:
    return f"{URI_SCHEME}{kind}/{ident}" if ident != "" else f"{URI_SCHEME}{kind}"


def parse(content_id: str | None) -> tuple[str, str]:
    """`pubky-music://album/12` -> ("album", "12"); the root is ("root", "")."""
    if not content_id or not content_id.startswith(URI_SCHEME):
        return "root", ""
    kind, _, ident = content_id[len(URI_SCHEME) :].partition("/")
    return kind or "root", ident


def _artists(track: dict) -> str:
    names = [a.get("name") if isinstance(a, dict) else str(a) for a in track.get("artists") or []]
    return ", ".join(n for n in names if n)


class Browser:
    """Builds BrowseMedia trees from the node's API. `thumb(kind, id)` gives a proxied image URL."""

    def __init__(self, hass: HomeAssistant, api: PubkyMusicApi, thumb) -> None:
        self.hass = hass
        self.api = api
        self.thumb = thumb

    def folder(
        self,
        kind: str,
        ident: Any,
        title: str,
        cls: MediaClass,
        *,
        can_play: bool = False,
        children_cls: MediaClass | None = None,
        thumbnail: str | None = None,
    ) -> BrowseMedia:
        return BrowseMedia(
            media_class=cls,
            media_content_id=uri(kind, ident),
            media_content_type=kind,
            title=title,
            can_play=can_play,
            can_expand=True,
            children_media_class=children_cls,
            thumbnail=thumbnail,
        )

    def album(self, a: dict) -> BrowseMedia:
        title = a.get("title") or "Untitled"
        who = a.get("album_artist")
        return BrowseMedia(
            media_class=MediaClass.ALBUM,
            media_content_id=uri("album", a["id"]),
            media_content_type=MediaType.ALBUM,
            title=f"{title} · {who}" if who else title,
            can_play=True,
            can_expand=True,
            children_media_class=MediaClass.TRACK,
            thumbnail=self.thumb("album", a["id"]),
        )

    def artist(self, a: dict) -> BrowseMedia:
        return BrowseMedia(
            media_class=MediaClass.ARTIST,
            media_content_id=uri("artist", a["id"]),
            media_content_type=MediaType.ARTIST,
            title=a.get("name") or "Unknown artist",
            can_play=True,
            can_expand=True,
            children_media_class=MediaClass.TRACK,
            thumbnail=self.thumb("artist", a["id"]),
        )

    def track(self, t: dict) -> BrowseMedia:
        who = _artists(t)
        title = t.get("title") or "Untitled"
        album = t.get("album") if isinstance(t.get("album"), dict) else None
        return BrowseMedia(
            media_class=MediaClass.TRACK,
            media_content_id=uri("track", t["id"]),
            media_content_type=MediaType.TRACK,
            title=f"{title} · {who}" if who else title,
            can_play=True,
            can_expand=False,
            thumbnail=self.thumb("album", album["id"]) if album and album.get("id") else None,
        )

    def playlist(self, p: dict) -> BrowseMedia:
        return BrowseMedia(
            media_class=MediaClass.PLAYLIST,
            media_content_id=uri("playlist", p["id"]),
            media_content_type=MediaType.PLAYLIST,
            title=p.get("name") or "Playlist",
            can_play=True,
            can_expand=True,
            children_media_class=MediaClass.TRACK,
        )

    async def browse(self, content_id: str | None) -> BrowseMedia:
        if content_id and media_source.is_media_source_id(content_id):
            return await media_source.async_browse_media(self.hass, content_id)
        kind, ident = parse(content_id)
        if kind == "root":
            root = self.folder("root", "", "Pubky Music", MediaClass.DIRECTORY, children_cls=MediaClass.DIRECTORY)
            root.children = [self.folder(k, "", t, MediaClass.DIRECTORY, children_cls=c) for k, t, c in LIBRARY]
            try:
                root.children.append(await media_source.async_browse_media(self.hass, None))
            except BrowseError:
                pass
            return root
        if kind == "home":
            node = self.folder("home", "", "Home", MediaClass.DIRECTORY, children_cls=MediaClass.DIRECTORY)
            node.children = [
                self.folder("home", key, title, MediaClass.DIRECTORY, children_cls=MediaClass.ALBUM)
                for key, title in HOME_ROWS
            ]
            if ident:
                home = await self.api.get("/library/home", limit=50)
                title = dict(HOME_ROWS).get(ident, ident)
                node = self.folder("home", ident, title, MediaClass.DIRECTORY, children_cls=MediaClass.ALBUM)
                node.children = [self.album(a) for a in home.get(ident, []) if a.get("id") is not None]
            return node
        if kind == "albums":
            node = self.folder("albums", "", "Albums", MediaClass.DIRECTORY, children_cls=MediaClass.ALBUM)
            node.children = [self.album(a) for a in await self.api.get("/library/albums", limit=LIMIT)]
            return node
        if kind == "artists":
            node = self.folder("artists", "", "Artists", MediaClass.DIRECTORY, children_cls=MediaClass.ARTIST)
            node.children = [self.artist(a) for a in await self.api.get("/library/artists", limit=LIMIT)]
            return node
        if kind == "playlists":
            node = self.folder("playlists", "", "Playlists", MediaClass.DIRECTORY, children_cls=MediaClass.PLAYLIST)
            node.children = [self.playlist(p) for p in await self.api.get("/playlists")]
            return node
        if kind == "genres":
            node = self.folder("genres", "", "Genres", MediaClass.DIRECTORY, children_cls=MediaClass.GENRE)
            node.children = [
                self.folder(
                    "genre",
                    g["id"],
                    g.get("name") or "Genre",
                    MediaClass.GENRE,
                    can_play=True,
                    children_cls=MediaClass.ALBUM,
                )
                for g in await self.api.get("/library/genres")
            ]
            return node
        if kind == "album":
            tracks = await self.api.get(f"/library/albums/{int(ident)}/tracks")
            node = self.folder(
                "album",
                ident,
                "Album",
                MediaClass.ALBUM,
                can_play=True,
                children_cls=MediaClass.TRACK,
                thumbnail=self.thumb("album", ident),
            )
            node.children = [self.track(t) for t in _tracks(tracks)]
            if node.children:
                first = _tracks(tracks)[0]
                album = first.get("album") if isinstance(first.get("album"), dict) else None
                if album and album.get("title"):
                    node.title = album["title"]
            return node
        if kind == "artist":
            a = await self.api.get(f"/library/artists/{int(ident)}")
            tracks = await self.api.get(f"/library/artists/{int(ident)}/tracks")
            name = (a.get("artist") or a).get("name") if isinstance(a, dict) else None
            node = self.folder(
                "artist",
                ident,
                name or "Artist",
                MediaClass.ARTIST,
                can_play=True,
                children_cls=MediaClass.ALBUM,
                thumbnail=self.thumb("artist", ident),
            )
            albums = a.get("albums", []) if isinstance(a, dict) else []
            node.children = [self.album(al) for al in albums if al.get("id") is not None]
            node.children += [self.track(t) for t in _tracks(tracks)]
            return node
        if kind == "genre":
            g = await self.api.get(f"/library/genres/{int(ident)}")
            node = self.folder(
                "genre",
                ident,
                (g.get("genre") or {}).get("name") or "Genre",
                MediaClass.GENRE,
                can_play=True,
                children_cls=MediaClass.ALBUM,
            )
            node.children = [self.album(a) for a in g.get("albums", []) if a.get("id") is not None]
            node.children += [self.track(t) for t in g.get("tracks", [])]
            return node
        if kind == "playlist":
            p = await self.api.get(f"/playlists/{ident}")
            summary = p.get("summary", {})
            node = self.folder(
                "playlist",
                ident,
                summary.get("name") or "Playlist",
                MediaClass.PLAYLIST,
                can_play=True,
                children_cls=MediaClass.TRACK,
            )
            node.children = [self.track(t) for t in p.get("tracks", []) if t]
            return node
        raise BrowseError(f"Unknown Pubky Music item {content_id}")

    async def search(self, query: str, classes: list[MediaClass] | None) -> list[BrowseMedia]:
        r = await self.api.get("/library/search", q=query, limit=25)
        out: list[BrowseMedia] = []
        want = set(classes or [MediaClass.ARTIST, MediaClass.ALBUM, MediaClass.TRACK])
        if MediaClass.ARTIST in want:
            out += [self.artist(a) for a in r.get("artists", []) if a.get("id") is not None]
        if MediaClass.ALBUM in want:
            out += [self.album(a) for a in r.get("albums", []) if a.get("id") is not None]
        if MediaClass.TRACK in want:
            out += [self.track(t) for t in r.get("tracks", [])]
        return out


def _tracks(v: Any) -> list[dict]:
    """A list of tracks, or `{"tracks": [...]}`."""
    if isinstance(v, dict):
        return v.get("tracks", [])
    return v or []
