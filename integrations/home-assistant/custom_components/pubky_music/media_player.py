"""One media_player per zone of the node."""

from __future__ import annotations

import logging
from typing import Any

from homeassistant.components import media_source
from homeassistant.components.media_player import (
    BrowseMedia,
    MediaPlayerEnqueue,
    MediaPlayerEntity,
    MediaPlayerEntityFeature,
    MediaPlayerState,
    MediaType,
    RepeatMode,
    SearchMedia,
    SearchMediaQuery,
    async_process_play_media_url,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.dispatcher import async_dispatcher_connect
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.util import dt as dt_util

from . import PubkyMusicNode
from .api import NodeError
from .browse import Browser, parse
from .const import DOMAIN, MA_SERVICES, SIGNAL_CONNECTION, SIGNAL_NEW_ZONE, SIGNAL_ZONE

_LOGGER = logging.getLogger(__name__)

BASE_FEATURES = (
    MediaPlayerEntityFeature.PAUSE
    | MediaPlayerEntityFeature.PLAY
    | MediaPlayerEntityFeature.STOP
    | MediaPlayerEntityFeature.NEXT_TRACK
    | MediaPlayerEntityFeature.PREVIOUS_TRACK
    | MediaPlayerEntityFeature.SEEK
    | MediaPlayerEntityFeature.PLAY_MEDIA
    | MediaPlayerEntityFeature.MEDIA_ENQUEUE
    | MediaPlayerEntityFeature.BROWSE_MEDIA
    | MediaPlayerEntityFeature.SEARCH_MEDIA
    | MediaPlayerEntityFeature.REPEAT_SET
    | MediaPlayerEntityFeature.SHUFFLE_SET
    | MediaPlayerEntityFeature.CLEAR_PLAYLIST
    | MediaPlayerEntityFeature.SELECT_SOURCE
    | MediaPlayerEntityFeature.GROUPING
)
VOLUME_FEATURES = (
    MediaPlayerEntityFeature.VOLUME_SET | MediaPlayerEntityFeature.VOLUME_STEP | MediaPlayerEntityFeature.VOLUME_MUTE
)
REPEAT_TO_HA = {"off": RepeatMode.OFF, "all": RepeatMode.ALL, "one": RepeatMode.ONE}
REPEAT_TO_NODE = {v: k for k, v in REPEAT_TO_HA.items()}


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    node: PubkyMusicNode = entry.runtime_data
    known: set[str] = set()

    @callback
    def add(zids: list[str]) -> None:
        new = [z for z in zids if z not in known and z in node.zones]
        known.update(new)
        if new:
            async_add_entities([ZonePlayer(node, z) for z in new])

    @callback
    def new_zone(zid: str) -> None:
        add([zid])

    add(list(node.zones))
    entry.async_on_unload(async_dispatcher_connect(hass, SIGNAL_NEW_ZONE.format(entry=entry.entry_id), new_zone))


class ZonePlayer(MediaPlayerEntity):
    """A zone: its queue, transport, volume (when its output has hardware volume) and groups."""

    _attr_has_entity_name = True
    _attr_name = None
    _attr_media_image_remotely_accessible = False

    def __init__(self, node: PubkyMusicNode, zone_id: str) -> None:
        self.node = node
        self.zone_id = zone_id
        self._attr_unique_id = f"{node.node_id}-{zone_id}"
        z = node.zones[zone_id]
        out = node.outputs.get(z.get("output"), {})
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, self._attr_unique_id)},
            name=z.get("name") or zone_id,
            manufacturer="Pubky Music",
            model=f"Zone ({out.get('protocol', 'output')})",
            via_device=(DOMAIN, node.node_id),
        )
        self._volume: float | None = None
        self._muted: bool | None = None
        self._position_at = None
        self._sources: dict[str, str] = {}
        self._source: str | None = None
        self.browser = Browser(node.hass, node.api, self._thumb)

    # ---- state -----------------------------------------------------------------------------------

    @property
    def zone(self) -> dict:
        return self.node.zones.get(self.zone_id, {})

    @property
    def available(self) -> bool:
        return self.node.connected and self.zone_id in self.node.zones

    async def async_added_to_hass(self) -> None:
        entry = self.node.entry.entry_id
        self.async_on_remove(
            async_dispatcher_connect(self.hass, SIGNAL_ZONE.format(entry=entry, zone=self.zone_id), self._updated)
        )
        self.async_on_remove(async_dispatcher_connect(self.hass, SIGNAL_CONNECTION.format(entry=entry), self._updated))
        await self._load_sources()
        self._position_at = dt_util.utcnow()

    @callback
    def _updated(self) -> None:
        if self.zone_id not in self.node.zones and self.node.connected:
            # The zone was deleted on the node: remove the entity
            registry = er.async_get(self.hass)
            if self.registry_entry:
                registry.async_remove(self.entity_id)
            return
        self._position_at = dt_util.utcnow()
        self.async_write_ha_state()

    @property
    def supported_features(self) -> MediaPlayerEntityFeature:
        out = self.node.outputs.get(self.zone.get("output"), {})
        if (out.get("caps") or {}).get("hw_volume"):
            return BASE_FEATURES | VOLUME_FEATURES
        return BASE_FEATURES

    @property
    def state(self) -> MediaPlayerState:
        t = self.zone.get("transport")
        if t == "playing":
            return MediaPlayerState.PLAYING
        if t == "paused":
            return MediaPlayerState.PAUSED
        return MediaPlayerState.IDLE

    @property
    def _item(self) -> dict:
        return (self.zone.get("now_playing") or {}).get("item") or {}

    @property
    def media_content_id(self) -> str | None:
        return self._item.get("uri")

    @property
    def media_content_type(self) -> str | None:
        return MediaType.MUSIC if self._item else None

    @property
    def media_title(self) -> str | None:
        return self._item.get("title") or self._item.get("uri")

    @property
    def media_artist(self) -> str | None:
        return self._item.get("artist")

    @property
    def media_album_name(self) -> str | None:
        return self._item.get("album")

    @property
    def media_duration(self) -> float | None:
        np = self.zone.get("now_playing") or {}
        d = np.get("duration_ms") or self._item.get("duration_ms")
        return d / 1000 if d else None

    @property
    def media_position(self) -> float | None:
        np = self.zone.get("now_playing")
        return np["position_ms"] / 1000 if np and np.get("position_ms") is not None else None

    @property
    def media_position_updated_at(self):
        return self._position_at if self.zone.get("now_playing") else None

    @property
    def media_image_url(self) -> str | None:
        image = self._item.get("image")
        if image:
            return image
        uri = self._item.get("uri") or ""
        if uri.startswith("track:"):
            return f"/v1/library/tracks/{uri[6:]}/cover"
        return None

    async def async_get_media_image(self) -> tuple[bytes | None, str | None]:
        url = self.media_image_url
        if not url:
            return None, None
        if url.startswith("/v1/library/tracks/"):
            try:
                t = await self.node.api.get(url[3:].removesuffix("/cover"))
            except NodeError:
                return None, None
            album = t.get("album") if isinstance(t.get("album"), dict) else None
            if not album or album.get("id") is None:
                return None, None
            url = f"/v1/library/albums/{album['id']}/cover"
        return await self.node.api.image(url)

    @property
    def repeat(self) -> RepeatMode:
        return REPEAT_TO_HA.get(self.zone.get("repeat"), RepeatMode.OFF)

    @property
    def shuffle(self) -> bool:
        # The node shuffles the rest of the queue once; it keeps no shuffle mode
        return False

    @property
    def volume_level(self) -> float | None:
        return self._volume

    @property
    def is_volume_muted(self) -> bool | None:
        return self._muted

    @property
    def source_list(self) -> list[str]:
        return list(self._sources)

    @property
    def source(self) -> str | None:
        return self._source

    @property
    def group_members(self) -> list[str] | None:
        out = self.zone.get("output") or ""
        if not out.startswith("group:"):
            return None
        members = out[len("group:") :].split("+")
        registry = er.async_get(self.hass)
        ids = [self.entity_id]
        for zid, z in self.node.zones.items():
            if zid != self.zone_id and z.get("output") in members:
                eid = registry.async_get_entity_id("media_player", DOMAIN, f"{self.node.node_id}-{zid}")
                if eid:
                    ids.append(eid)
        return ids

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        z = self.zone
        return {
            "zone_id": self.zone_id,
            "output": z.get("output"),
            "queue_length": len(z.get("queue") or []),
            "radio": z.get("radio"),
            "error": z.get("error"),
        }

    # ---- control ---------------------------------------------------------------------------------

    async def _call(self, method: str, path: str, body: Any = None) -> Any:
        try:
            return await self.node.api.request(method, path, json=body)
        except NodeError as err:
            raise HomeAssistantError(str(err)) from err

    async def _transport(self, action: str, **extra: Any) -> None:
        await self._call("POST", f"/zones/{self.zone_id}/transport", {"action": action, **extra})

    async def async_media_play(self) -> None:
        await self._transport("play")

    async def async_media_pause(self) -> None:
        await self._transport("pause")

    async def async_media_stop(self) -> None:
        await self._transport("stop")

    async def async_media_next_track(self) -> None:
        await self._transport("next")

    async def async_media_previous_track(self) -> None:
        await self._transport("previous")

    async def async_media_seek(self, position: float) -> None:
        await self._transport("seek", position_ms=int(position * 1000))

    async def async_set_repeat(self, repeat: RepeatMode) -> None:
        await self._call("PUT", f"/zones/{self.zone_id}/repeat", {"repeat": REPEAT_TO_NODE[repeat]})

    async def async_set_shuffle(self, shuffle: bool) -> None:
        if shuffle:
            await self._call("POST", f"/zones/{self.zone_id}/shuffle", {})

    async def async_clear_playlist(self) -> None:
        await self._call("DELETE", f"/zones/{self.zone_id}/queue")

    async def _volume_call(self, volume: Any) -> None:
        await self._call("POST", f"/zones/{self.zone_id}/volume", {"volume": volume})

    async def async_set_volume_level(self, volume: float) -> None:
        await self._volume_call({"percent": round(volume * 100)})
        self._volume = volume
        self.async_write_ha_state()

    async def async_volume_up(self) -> None:
        await self._volume_call("up")

    async def async_volume_down(self) -> None:
        await self._volume_call("down")

    async def async_mute_volume(self, mute: bool) -> None:
        await self._volume_call({"mute": mute})
        self._muted = mute
        self.async_write_ha_state()

    # ---- source = the play-from order ------------------------------------------------------------

    async def _load_sources(self) -> None:
        try:
            po = await self.node.api.get("/play-order")
        except NodeError:
            return
        names = {s["id"]: s["name"] for s in po.get("services", []) if s.get("connected")}
        self._sources = {names[i]: i for i in po.get("order", []) if i in names}
        for sid, name in names.items():
            self._sources.setdefault(name, sid)
        first = next((i for i in po.get("order", []) if i in names), None)
        self._source = names.get(first) if first else None

    async def async_select_source(self, source: str) -> None:
        sid = self._sources.get(source)
        if not sid:
            raise ServiceValidationError(f"{source} is not one of this node's sources")
        po = await self._call("GET", "/play-order")
        order = [sid] + [i for i in po.get("order", []) if i != sid]
        await self._call("PUT", "/play-order", {"order": order})
        await self._load_sources()
        self.async_write_ha_state()

    # ---- grouping: a group zone of the members' outputs ------------------------------------------

    def _zone_of_entity(self, entity_id: str) -> str | None:
        ent = er.async_get(self.hass).async_get(entity_id)
        prefix = f"{self.node.node_id}-"
        if ent and ent.platform == DOMAIN and ent.unique_id.startswith(prefix):
            return ent.unique_id[len(prefix) :]
        return None

    async def async_join_players(self, group_members: list[str]) -> None:
        members = [self._zone_of_entity(e) for e in group_members]
        if None in members:
            raise ServiceValidationError("Only Pubky Music zones of the same node can be grouped")
        outputs = [self.zone.get("output")] + [
            self.node.zones[m].get("output") for m in members if m in self.node.zones
        ]
        outputs = [o for o in dict.fromkeys(outputs) if o and not o.startswith("group:")]
        if len(outputs) < 2:
            raise ServiceValidationError("Pick at least one other zone")
        names = [self.zone.get("name")] + [self.node.zones[m].get("name") for m in members if m in self.node.zones]
        group = await self._call("POST", "/zones", {"name": " + ".join(n for n in names if n), "outputs": outputs})
        gid = group.get("id") if isinstance(group, dict) else None
        if gid and self.zone.get("queue"):
            await self._call("POST", f"/zones/{self.zone_id}/transfer", {"to": gid})

    async def async_unjoin_player(self) -> None:
        out = self.zone.get("output") or ""
        if out.startswith("group:"):
            leader = out[len("group:") :].split("+")[0]
            back = next((zid for zid, z in self.node.zones.items() if z.get("output") == leader), None)
            if back and self.zone.get("queue"):
                await self._call("POST", f"/zones/{self.zone_id}/transfer", {"to": back})
            await self._call("DELETE", f"/zones/{self.zone_id}")
            return
        # A member: leave the group zone it belongs to (the rest stay grouped)
        mine = self.zone.get("output")
        for zid, z in list(self.node.zones.items()):
            gout = z.get("output") or ""
            if gout.startswith("group:") and mine in gout[len("group:") :].split("+"):
                rest = [o for o in gout[len("group:") :].split("+") if o != mine]
                if len(rest) >= 2:
                    newz = await self._call("POST", "/zones", {"name": z.get("name"), "outputs": rest})
                    if z.get("queue") and isinstance(newz, dict):
                        await self._call("POST", f"/zones/{zid}/transfer", {"to": newz["id"]})
                elif z.get("queue"):
                    back = next((i for i, x in self.node.zones.items() if x.get("output") == rest[0]), None)
                    if back:
                        await self._call("POST", f"/zones/{zid}/transfer", {"to": back})
                await self._call("DELETE", f"/zones/{zid}")

    # ---- play_media ------------------------------------------------------------------------------

    async def async_play_media(
        self, media_type: MediaType | str, media_id: str, enqueue: MediaPlayerEnqueue | None = None, **kwargs: Any
    ) -> None:
        title = None
        if media_source.is_media_source_id(media_id):
            item = await media_source.async_resolve_media(self.hass, media_id, self.entity_id)
            media_id = async_process_play_media_url(self.hass, item.url)
            title = media_id.rsplit("/", 1)[-1].split("?")[0]
        replace = enqueue in (None, MediaPlayerEnqueue.PLAY, MediaPlayerEnqueue.REPLACE)
        at = "next" if enqueue == MediaPlayerEnqueue.NEXT else "end"
        kind, ident = parse(media_id)
        if kind == "playlist":
            await self._call("POST", f"/playlists/{ident}/play", {"zone": self.zone_id, "replace": replace})
            return
        body: dict[str, Any] = {"replace": replace, "play": replace, "at": at}
        if kind == "track":
            body["track_ids"] = [int(ident)]
        elif kind == "album":
            body["track_ids"] = [
                t["id"] for t in _tracks(await self._call("GET", f"/library/albums/{int(ident)}/tracks"))
            ]
        elif kind == "artist":
            body["track_ids"] = [
                t["id"] for t in _tracks(await self._call("GET", f"/library/artists/{int(ident)}/tracks"))
            ]
        elif kind == "genre":
            g = await self._call("GET", f"/library/genres/{int(ident)}")
            body["track_ids"] = [t["id"] for t in g.get("tracks", [])]
        elif media_id.startswith(("http://", "https://")):
            body["items"] = [{"uri": media_id, **({"title": title} if title else {})}]
        else:
            body["items"] = [{"uri": _node_uri(media_id)}]
        if not body.get("track_ids") and not body.get("items"):
            raise ServiceValidationError(f"Nothing to play in {media_id}")
        await self._call("POST", f"/zones/{self.zone_id}/queue", body)

    # ---- the media browser -----------------------------------------------------------------------

    def _thumb(self, kind: str, ident: Any) -> str | None:
        return self.get_browse_image_url(kind, str(ident))

    async def async_get_browse_image(
        self, media_content_type: str, media_content_id: str, media_image_id: str | None = None
    ) -> tuple[bytes | None, str | None]:
        if media_content_type == "album":
            return await self.node.api.image(f"/v1/library/albums/{int(media_content_id)}/cover")
        if media_content_type == "artist":
            return await self.node.api.image(f"/v1/library/artists/{int(media_content_id)}/image")
        return None, None

    async def async_browse_media(
        self, media_content_type: MediaType | str | None = None, media_content_id: str | None = None
    ) -> BrowseMedia:
        try:
            return await self.browser.browse(media_content_id)
        except NodeError as err:
            raise HomeAssistantError(str(err)) from err

    async def async_search_media(self, query: SearchMediaQuery) -> SearchMedia:
        try:
            return SearchMedia(result=await self.browser.search(query.search_query, query.media_filter_classes))
        except NodeError as err:
            raise HomeAssistantError(str(err)) from err


def _tracks(v: Any) -> list[dict]:
    if isinstance(v, dict):
        return v.get("tracks", [])
    return v or []


def _node_uri(media_id: str) -> str:
    """The node's own URIs pass through; Music Assistant style `tidal://track/1` becomes
    `tidal:track:1` for the services the node has."""
    scheme, sep, rest = media_id.partition("://")
    if sep and scheme in MA_SERVICES and "/" in rest:
        kind, _, ident = rest.partition("/")
        return f"{scheme}:{kind}:{ident}"
    return media_id
