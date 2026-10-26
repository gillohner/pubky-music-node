# The sources vault, v1

A person's streaming sign-ins (TIDAL, Qobuz, Spotify, …) live on their own homeserver, encrypted on the
device, so every client they authorise plays from the same sign-ins. The homeserver only stores ciphertext:
`/priv/` is access-controlled, not encrypted, and its operator can read it. Reference code:
`pubky_music_specs::vault` (feature `vault`); vectors: `tests/vectors/vault-v1.json`, with an independent
Python check of them (`tests/vectors/check_vault_v1.py`) for implementers in other languages.

## Paths

| Path | What |
| :-- | :-- |
| `/priv/pubky.music/vault/index.json` | the device list (plain JSON; every entry proves itself) |
| `/priv/pubky.music/vault/<service>.json` | one sign-in, encrypted (the envelope below) |
| `/priv/pubky.music/vault/<service>.lease.json` | refresh lease, only where locks are refused (below) |

`<service>` names one sign-in: the source id the node uses (`tidal`, `qobuz`, …), with a suffix where a
service has two kinds that coexist (`tidal-full` for TIDAL's device client, `spotify-librespot`). It is 1–32 of
`a-z`, `0-9`, `-`, never `index`.

## Clients and their keys

Every client (node, desktop app, phone, browser app) makes a random X25519 static key once and keeps the
secret on the device. It is authorised by the person's identity, without ever holding the identity secret:

1. The person approves the client in Pubky Ring with a grant that names the vault: the capability
   `/priv/pubky.music/vault/:rw`. An ancestor (`/:rw`, `/priv/pubky.music/:rw`) is not enough. Ring showed the
   person "all your storage" for such an app, not "your streaming sign-ins", so a broad grant never makes an
   app a vault client.
2. The client signs a **custom proof** with its grant's client key (pubky SDK
   `GrantSessionView::create_custom_pop`), with this data:
   `{"kind": "pubky.music.vault-client", "v": 1, "client_key": "<hex X25519 public key>", "name": "<label>",
   "device": "node"}`. `device` is optional: `node`, `desktop`, `android`, `ios` or `browser` (for the icon;
   readers that don't know a value show a generic one).
   The result, `{grant, pop}`, is the root-signed grant JWS (it names the client key in `cnf`) and the proof JWS.
3. It adds itself to `index.json`, under that file's lock (see "Editing `index.json`").

A reader trusts an entry only if all of these hold:
- `verify_custom_grant_pop` accepts the proof (both signatures, the grant bound to the proof, the grant
  not expired);
- the grant's `iss` is the vault's owner;
- the grant's capabilities include `/priv/pubky.music/vault/` itself with `r` and `w`;
- `data.kind` and `data.v` are as above;
- `data.client_key` equals the entry's `client_key`;
- `data.name` equals the entry's `name`.

The homeserver can't forge an entry, and the last rule stops it from renaming one ("Living room node" shown as
"Your phone" on the entry the person then signs out). Readers show the signed `data.name` and `data.device`;
`last_used_at` isn't signed and is only a hint. A client re-publishes its proof when its grant is renewed. An
entry whose grant has expired stays listed but gets no new keys; clients may show the grant's `exp` (in the
grant JWS) to say it needs approving again in Ring.

Why not Paykit's record (`paykit.noise_key_authorization`): it is signed by the identity key itself, so some
app must hold the root secret. A grant proof gives the same identity binding with only what Ring already does.

```json
{ "version": 1,
  "clients": [ { "client_key": "<hex>", "name": "Living room node", "authorised_at": 1761400000,
                 "last_used_at": 1761400000, "proof": { "grant": "<JWS>", "pop": "<JWS>" } } ],
  "revoked": [ { "client_key": "<hex>", "grant_id": "<jti>", "at": 1761500000 } ] }
```
Times are Unix seconds. `last_used_at` is written at most once a day per client. A client listed in
`revoked` (by key or grant id) never gets keys again, even if it re-adds itself.

**Editing `index.json`.** Every change to the list (adding a client, `last_used_at`, signing one out)
is a read-modify-write under that file's lock: `LOCK` on `index.json`, read it again, change it, `PUT`
with the lock, unlock. Where `LOCK` answers `405`, use the lease (`index.lease.json`, as for blobs
below). An edit made without the lock can drop a registration or a sign-out made at the same time.

**Revocation is remembered by each client.** The `revoked` list is not tamper-proof. Every client
listed in `index.json` can write it, a signed-out one too (its homeserver grant still allows that),
and the homeserver can serve an older copy. So:
- each client keeps its own append-only record: the sign-outs it made itself, never dropped, and the
  well-formed `revoked` entries it has seen (naming a key, or a grant id of the SDK's form), up to a bound
  (4096 in the node) so a flood of made-up ones only fills what is left;
- it doesn't remember records that name itself: it follows the list while the list says so, so a writer
  can't sign it out for good;
- it trusts nobody in the union of that record and the list as served;
- when it writes `index.json`, it writes back the remembered records that name a listed entry (the only
  ones that change anyone's trust), so a dropped record returns; and when the list would pass 256 KiB, it
  first drops records that name no listed entry.

A list that drops a record therefore re-trusts nobody for a client that has seen the record. The same
holds for epochs: clients remember the highest epoch they have seen.

**Who gets a key is decided under the blob's lock.** A client that wraps a data key to the trusted
clients (writing a blob, or adding a device) reads `index.json` again after it takes that blob's lock.
A sign-out changes the index before it takes any blob lock, so a client signed out meanwhile is never
wrapped to.

## The envelope

```json
{ "version": 1, "epoch": 3, "nonce": "<b64>", "ciphertext": "<b64>",
  "recipients": [ { "client_key": "<hex>", "epk": "<b64>", "wrapped_key": "<b64>" } ] }
```
`<b64>` is unpadded base64url; keys are lowercase hex (as Paykit's `noise_static_public_key`).

- **Data key:** 32 random bytes per blob, new at every rotation (`epoch` + 1, see Revocation). Each write uses
  a fresh random 24-byte `nonce`.
- **Content:** `ciphertext = XChaCha20-Poly1305(data_key, nonce, aad, plaintext)` with
  `aad = "pubky.music/vault/v1/blob" 0x00 owner 0x00 path 0x00 epoch`.
- **Wrap per recipient:** take a fresh ephemeral X25519 secret, `epk = X25519(eph, base)` and
  `shared = X25519(eph, client_key)`. An all-zero `shared` is refused. Then
  `wrap_key = HKDF-SHA-256(salt = none, ikm = shared, info = "pubky.music/vault/v1/wrap" 0x00 epk client_key
  owner 0x00 path 0x00 epoch)` and `wrapped_key = XChaCha20-Poly1305(wrap_key, 24 zero bytes, "", data_key)`.
  Each wrap key encrypts one thing, once, so its nonce can be fixed.
- **Field encodings in `aad` and `info`:**
  - `owner`: the owner's z32 public key, UTF-8;
  - `path`: the blob's full path, e.g. `/priv/pubky.music/vault/tidal.json`;
  - `epoch`: 8 bytes, big-endian;
  - `epk`, `client_key`: 32 raw bytes.
- **Primitives:** X25519, HKDF-SHA-256 and XChaCha20-Poly1305, as in pubky-noise (`curve25519-dalek`, `hkdf`,
  `chacha20poly1305`). Nothing new is invented. When Pubky ships one shared `priv` envelope, v2 moves to it.

**Plaintext**, UTF-8 JSON. Members a reader doesn't know are kept when it rewrites the blob.
```json
{ "service": "tidal", "label": "gil@example.org", "way": "app", "client_id": "…",
  "tokens": { "access_token": "…", "refresh_token": "…", "token_type": "bearer" },
  "user": { "id": "1234", "name": "Gil", "country": "CH" },
  "scopes": ["r_usr", "w_usr"], "expires_at": 1761403600, "created_by": "<hex>",
  "last_refresh": { "client": "<hex>", "at": 1761400000 }, "revision": 12, "settings": { "quality": "max" } }
```
- `way` is the id of the Sign in chooser's way it was made with: `app` (the project's app), `device` (a
  device-code client), `own` (the person's own app), `import` (with `from`, the app it came from, e.g.
  `tidal-dl-ng`) or `paste`. Readers that don't know a `way` still use the tokens.
- `tokens.token_type` is `bearer` (an OAuth access token, often with a `refresh_token`), `session_key` (Last.fm)
  or `api_key` (a single key, e.g. YouTube Data). Sign-ins that sign every request with the person's own app
  keep its secret in `client_secret`, next to `client_id` (Qobuz, Last.fm). That secret is the person's, in
  their own vault; built-in apps' secrets are never written here.
- `user`: the account as the service names it (`id`, and `name`, `country` where it has them).
- `expires_at` absent or `null`: the token doesn't expire (Qobuz, Last.fm session keys).
- A client that can't refresh this sign-in, because it lacks the issuing app's credentials (a browser
  client with a device-client sign-in), uses the tokens until `expires_at` and leaves the refresh to another
  client.
- `revision` goes up by one at every write.
- `settings` holds what should follow the person across clients: `quality` takes the ids of
  `GET/PUT /v1/sources/<id>/quality` (TIDAL `max|high`, Qobuz `max|cd|mp3`, …).
- `label` can be an e-mail address. It stays in the blob: the device list's `name` is a device's name, never
  copied from `label`.
- Credentials kept as a whole file on one device (librespot's `credentials.json`) stay on that device in v1.

## Adding a device, revocation

- **Adding a device:** a client that sees a trusted entry with no recipient in a blob unwraps the data key and
  appends a recipient for it, keeping the epoch, under that blob's lock.
- **Sign out this device:**
  1. The entry moves to `revoked` (under the index lock). Where Ring offers it, the person also revokes
     the device's grant, so it loses write access to the vault folder.
  2. Every blob gets a new data key at `epoch` + 1, is encrypted again, and is wrapped only to the remaining
     trusted clients.
  3. The token is revoked at the service where its API allows that.
  - A removed device can still read what it saw before, nothing after.
  - One sign-in that can't be rotated now (locked by another client's refresh, unreadable) doesn't stop the
    others: it is reported, and its next write leaves the device out anyway (writers read the index under the
    blob's lock and never wrap to a signed-out client).
  - A client never signs itself out through its own grant id: when the entry being signed out shares the
    signer's grant (an old key of the same client), only its key goes into `revoked`.
- **Sign out everywhere:** delete the blobs, each under its lock, and revoke each token at its service.

Clients remember the highest `epoch` and `revision` they have seen per blob. They don't write over a newer
blob, and they treat an older one as stale and read it again (a homeserver can serve an old copy).

## Refreshing a token

Several clients hold one sign-in, and many services rotate the refresh token, so only one refreshes at a time:

1. A client whose token is about to expire (60 s ahead) or was refused takes the homeserver's write lock on the
   blob: `LOCK /storage/<owner>/priv/pubky.music/vault/<service>.json` with `Timeout: Second-30`, renewed
   every 10 s. This is the pubky SDK's `SessionStorage::lock` on homeservers from 0.16.2.
2. Under the lock it reads the blob again. If another client has already refreshed it (higher `revision`, not
   expired), it uses that and unlocks.
3. Otherwise it refreshes with the refresh token it just read, giving the service at most 60 s (then it unlocks
   and gives up). It writes the new tokens with `revision` + 1 and `last_refresh` (`PUT` with the lock's `If`
   header), then unlocks.
   - It reads the blob again just before writing. If another client's fresh copy is there (newer, other tokens,
     not due), that one stands. If the blob was changed meanwhile without new tokens (a device added, a
     sign-out rotation, a setting), it puts its tokens into that copy, wrapped to that copy's recipients.
   - The new tokens may be the only ones that still work (the service has used up the old refresh token), so
     they are never dropped: a failed write is tried again, taking the lock again if it lapsed. If it still
     fails, the client keeps using them and writes them later.
4. A client that finds the blob locked (`423`) retries the lock for about 2 s, then waits 1–2 s and reads the
   blob again. It never refreshes a token it didn't just read under the lock.
5. A renewal that fails while the service answers is tried again at the next tick (the lock still has up to
   20 s). If the lock has lapsed by the time of the write (`412`), the client locks again and writes by the
   rules of step 3. At a service that rotates refresh tokens a lapsed lock can cost one refresh: another
   client that took the lock meanwhile fails at the service, and its next read picks up these tokens.
   Each renewal and each write try is time-limited, so a stalled connection never holds the new tokens.

**Where `LOCK` is refused** (`405`, an older homeserver), the lease file takes its place:
1. Read `<service>.lease.json` (`{"client": "<hex>", "until": <unix s>}`). If another client's lease is live,
   wait and read the blob again.
2. Otherwise write your own lease (`until` = now + 30), wait 2 s, and read the lease again. Go on only if it is
   still yours.
3. Refresh, renewing the lease (`until` = now + 30) every 10 s while the service answers, write the blob, then
   delete the lease. Delete it only while it is yours and still live: once it has run out another client may
   be taking it.

Leases compare `until` with the local clock, so a client whose clock is more than 30 s off can take a live
lease. That costs a failed refresh at the service. Locks don't depend on clocks.

The lease is its own file, not a member of the blob: writing a lease into the blob would race with another
client's token write and could undo a rotated refresh token.

## Limits

Blob ≤ 64 KiB, index ≤ 256 KiB, ≤ 64 clients. Larger answers are refused unread.

A reader still reads an `index.json` that lists more than 64 entries, within the byte limit, so entries
that don't verify can't make the list unreadable. A client adding itself to a full list first drops the
entries that aren't trusted. Their sign-outs stay in `revoked`.
