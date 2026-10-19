# Test fixtures

FLAC files from the IETF CELLAR working group's FLAC test suite,
<https://github.com/ietf-wg-cellar/flac-test-files> (`subset/`), licensed
**CC0 1.0 Universal** (public domain dedication, see that repo's LICENSE.txt).
Fetched 2026-10-19 from `raw.githubusercontent.com/ietf-wg-cellar/flac-test-files/main/subset/`.

| File | Upstream name | Size | Why |
| :-- | :-- | --: | :-- |
| `60_mono_audio.flac` | 60 - mono audio.flac | 47 KB | mono layout |
| `61_predictor_overflow_check_16-bit.flac` | 61 - predictor overflow check, 16-bit.flac | 117 KB | 16-bit, full-scale extremes |
| `63_predictor_overflow_check_24-bit.flac` | 63 - predictor overflow check, 24-bit.flac | 204 KB | 24-bit, full-scale extremes |
| `14_wasted_bits.flac` | 14 - wasted bits.flac | 232 KB | wasted-bits subframes (decoder shift path) |

Each file carries a STREAMINFO MD5 of the decoded PCM; the tests decode with
verification on and assert the MD5 matches, which proves bit-exact decoding.

No ALAC (`.m4a`) fixture is included: there is no encoder on the build
machine and no small CC0 ALAC sample was found. ALAC decoding is covered by
code review of symphonia's output justification only (see `decode.rs` docs).
