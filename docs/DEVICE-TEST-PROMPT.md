# Device test prompt: Chromecast, Google TV, smart TVs, Sonos, AirPlay, Snapcast

The multiroom outputs (infra brief §3) were built against mocks and open-source receivers on a machine
with no speakers or TVs. Paste the section for each device you have into Claude Code on a laptop on
the same LAN as the node, or follow it by hand. Each section lists what was already checked without
the device, so you know what is new.

```
Use a node from main (or the edge image) with [api] token set, as in a real setup. In node.toml:
[outputs] upnp = true, cast = true. Report pass/fail with a screenshot or log line per check, and
each device's brand, model, year and firmware. Run the node with RUST_LOG=info,pm_out_cast=debug,
pm_out_upnp=debug and attach the log for failures.
```

## Chromecast, Google TV, Android TV, Nest speakers, Cast groups

Checked without devices (`crates/pm-out-cast` tests against a fake receiver):
- launch, load and stop;
- keepalive pings both ways;
- volume through the live link;
- resuming the same session after the connection drops;
- stepping aside when another app takes the device;
- the title, artist, album and art the device is sent;
- stream URLs that carry a per-stream key, not the token.

```
1. Settings → Outputs → Scan the network: every Cast device appears as a zone named after the
   device (model in brackets). Cast groups from the Google Home app appear as "<name> (Cast group)".
2. Play an album with covers to a Chromecast on a TV (or a Google TV): the TV shows the title,
   artist, album and cover; audio plays; the zone's position advances. Note the delay between
   pressing play and hearing it.
3. Pause and resume, next, previous, seek to the middle of a track: each is heard within a few
   seconds; the TV keeps playing (no "nothing is playing" screen) across a seek.
4. Volume slider in the zone: the TV's volume overlay moves. Mute.
5. Leave it playing 10 minutes: it does not stop by itself (the keepalive works).
6. Turn the router's Wi-Fi off for 5 s and on again while it plays: playback comes back without
   touching anything (log: "Cast session resumed").
7. While the node plays, cast YouTube to the same TV from a phone: the node's zone stops quietly
   and does not stop YouTube (log: "another sender took the Cast device").
8. A Cast group of two Nest/Home speakers: play to the group zone; both speakers play in sync.
9. A 24/96 FLAC: the signal path shows the resample to 48 kHz (Cast's ceiling), not bit-perfect.
10. Gapless album (live album or a DJ mix): no gaps between tracks. The TV keeps the first
    track's title for the whole run, a known limitation: a new title would need a reload, which
    breaks the gap.
```

## Smart TVs and other DLNA renderers (Samsung, LG, Sony, Philips)

Checked without devices, with gmediarender (a real DLNA renderer) on a token-protected node:
- Found over SSDP; fetched the keyed stream URL and played in real time.
- Limited to LPCM only (`--mime-filter audio/L16`, like many LG and Samsung TVs): offered DLNA LPCM.
  The zone converted to 16-bit 48 kHz and the signal path says so.
- Limited to MP3 only: no zone, and the log says why.
- The answer carries `transferMode.dlna.org` and `contentFeatures.dlna.org`, and HEAD does not take
  the stream away from the GET that follows.

```
1. The TV appears as a zone after a scan. TVs often need "DLNA", "Media sharing" or "Allow remote
   control" turned on, and must be on, not in standby. The node log line "UPnP renderer … stream="
   says what it chose (Wav or L16); "accepts no PCM stream" means the TV takes only compressed
   files, and no zone appears.
2. Play an album with covers: audio from the TV speakers; the TV shows title, artist, album and
   cover (photo it).
3. Pause/resume, next, seek, volume. Note anything the TV ignores or answers with an error (log
   lines from pm_out_upnp: "renderer Play refused, retrying" is fine once or twice).
4. A 24/96 file: on a WAV TV the signal path stays bit-perfect up to what the TV listed; on an
   LPCM TV it shows the conversion to 16-bit 44.1/48 kHz.
5. Report the TV's GetProtocolInfo list (in the log at debug level, or from
   `curl` on its ConnectionManager) so the device database can record it.
```

## Sonos

Checked without devices (`crates/pm-out-upnp/src/sonos.rs` tests against two fake players):
- A player in a Sonos group plays through its coordinator. The topology is read from
  `GetZoneGroupState`, escaped as Sonos sends it.
- A node zone made of Sonos players joins them with `x-rincon:` and they leave again on stop. The
  lead stops before the others leave.
- Sonos's ceiling is 16-bit up to 48 kHz.

```
1. Every Sonos player appears as a zone after a scan (UPnP on, same LAN; Sonos S1 and S2 both).
2. Play an album to one player: audio plays, the Sonos app shows the title, artist and cover.
   Check the signal path: anything above 16/48 shows the conversion.
3. Group two players in the Sonos app, then play from the node to the member (not the first
   one): the whole Sonos group plays (it went to the coordinator).
4. Ungroup them in the Sonos app. In the node, make a zone of both (Zones → new zone with two
   outputs) and play: the Sonos app shows them grouped, in sync. Stop: they are separate again.
5. Leave a zone playing for an hour: Sonos does not stop the stream by itself (it is a live WAV
   of unknown length; if Sonos stops near 4 GB or after some time, report when).
6. Volume per player.
```

## Snapcast (Raspberry Pis, old phones, any snapclient)

Checked here with real snapserver 0.34 and snapclient:
- Every Snapcast group became a zone, labelled by its clients.
- Playing routed the group to the node's stream; the client received the tone in real time.
- A test pattern came out of snapclient bit-exact at 16 and 24 bits (FLAC on the wire).
- Stopping put the group back on its previous stream and removed the node's.

```
1. Install snapserver on the node's machine (or any machine on the LAN) and snapclient on two
   Pis. In node.toml: [outputs] snapcast = "<snapserver host>". Restart the node: each Snapcast
   group is a zone "<client names> (Snapcast)".
2. Group both Pis in Snapcast (snapweb or the Snapcast app) and play to that zone: both play, in
   tight sync (stand between them: no echo).
3. Volume from the node changes both clients. Stop: the group goes back to what it played before.
4. A 24/96 album: the signal path stays lossless to 24/96; note if a Pi's sound card resamples
   (snapclient log).
```

## AirPlay

Not built yet: a section follows with the output.
