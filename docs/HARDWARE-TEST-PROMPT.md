# M1 hardware test: prompt for a Claude Code session on the node (or a laptop with a USB DAC)

Paste the text below into Claude Code on the machine that has the DAC. It assumes the repo is
present (clone it from GitHub once pushed, or `git clone pubky-music-node.bundle`).

---

Read README.md, docs/adr/0004-sample-representation.md and crates/pm-out-local/src/alsa.rs in
this repo first. The pm-out-local ALSA output is an uncompiled draft written on a machine without
libasound; your job is M1's hardware acceptance from the handoff: "play to a USB DAC via ALSA
`hw:` with byte-identical output against the source PCM, signal path reported". Do not change the
pipeline or the sample layout to make a test pass; if the draft is wrong, fix the ALSA code and
say what was wrong.

Setup
1. Install the ALSA headers (Debian/Ubuntu: `sudo apt install libasound2-dev alsa-utils`;
   Fedora: `alsa-lib-devel alsa-utils`). Build with `cargo build -p pm-node --features alsa` and
   fix compile errors in crates/pm-out-local/src/alsa.rs (alsa crate 0.9 API). Keep
   `cargo clippy -p pm-out-local --features alsa -- -D warnings` clean.
2. Plug in the USB DAC (a FiiO KA-series is expected). `aplay -l` must list it; note its card
   number N. Read `/proc/asound/cardN/stream0` and record every PCM rate, format and DSD
   capability it advertises; put them in a new docs/device-db/fiio-<model>.yaml using the schema
   in docs/device-db/README.md.
3. Free the device from the desktop audio server, otherwise `hw:` opens fail with EBUSY:
   PipeWire: `systemctl --user stop pipewire pipewire-pulse wireplumber` (restart them at the
   end); PulseAudio: `pactl suspend-sink <sink> 1` or `pasuspender -- <cmd>`. Also disable USB
   autosuspend for the DAC while testing (`echo on | sudo tee /sys/bus/usb/devices/<dev>/power/control`)
   and say whether it mattered.

Checks (report each as pass/fail with the evidence)
A. `pm-node outputs` lists the DAC as `alsa:hw:N,0` with caps that match step 2.
B. Play a 16/44.1 FLAC, a 24/96 FLAC and, if the DAC takes it, a 24/192 FLAC and a DSF file
   (use real files from a library; never a converted copy): `pm-node play <file> --to alsa:hw:N,0`.
   While each plays, `cat /proc/asound/cardN/pcm0p/sub0/hw_params` must show the source's
   format (S16_LE / S24_3LE or S32_LE / DSD_U8 or the DoP carrier), rate and channels; the
   printed signal path must be BitPerfect (or Lossless with exactly one `pad` or `dop` stage),
   and the DAC's own rate indicator (LED colour or display on the KA) must show the source rate.
   Any `Lossy` stage, any rate on the device that differs from the source, or any ALSA plugin in
   the path is a fail.
C. Byte-identical loopback: `sudo modprobe snd-aloop`, then in one terminal
   `arecord -D hw:Loopback,1,0 -f S24_3LE -r 96000 -c 2 -t raw cap.raw` (match the file's
   format) and in another `pm-node play <24-96.flac> --to alsa:hw:Loopback,0,0`. Decode the
   same file with `pm-node play <file> --to wav:ref.wav`, strip the WAV header, and compare the
   PCM bytes (`cmp` after trimming the capture's leading silence; document the trim). Repeat for
   16/44.1. The bytes must be identical.
D. Gapless: play two consecutive tracks of one album to the DAC and listen for a gap or click;
   report what the current code does (the zone/queue layer does not exist yet, so a gap is
   expected; say so rather than hiding it).
E. Volume: the KA exposes a UAC2 volume control; `amixer -c N` lists it. The draft reports
   `hw_volume: None`. If time allows, wire `set_volume` to that mixer control and verify with
   `amixer`; otherwise leave a TODO with the control's name.

Report back with: the fixed ALSA code diff summary, the hw_params captures, the cmp results,
the device-db YAML, and anything that could not be checked. Commit on a branch
`m1-hardware-<dac>`; do not push unless told to.
