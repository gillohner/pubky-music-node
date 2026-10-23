"""Constants for the Pubky Music integration."""

DOMAIN = "pubky_music"
DEFAULT_PORT = 7788
# Content ids in the media browser and for play_media: pubky-music://<kind>/<id>
URI_SCHEME = "pubky-music://"
# Music Assistant style URIs we map to the node's own (`tidal://track/1` -> `tidal:track:1`)
MA_SERVICES = ("tidal", "qobuz", "deezer", "soundcloud", "jamendo", "youtube")
SIGNAL_ZONE = f"{DOMAIN}_zone_{{entry}}_{{zone}}"
SIGNAL_NEW_ZONE = f"{DOMAIN}_new_zone_{{entry}}"
SIGNAL_CONNECTION = f"{DOMAIN}_connection_{{entry}}"
