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

`<service>` is the source id the node uses (`tidal`, `qobuz`, …): 1–32 of `a-z`, `0-9`, `-`.

## Clients and their keys

Every client (node, desktop app, phone, browser app) makes a random X25519 static key once and keeps the
secret on the device. It is authorised by the person's identity, without ever holding the identity secret:

1. The person approves the client in Pubky Ring with a grant whose capabilities cover the vault (a capability
   on `/priv/pubky.music/vault/` or an ancestor, with `r` and `w`).
2. The client signs a **custom proof** with its grant's client key (pubky SDK
   `GrantSessionView::create_custom_pop`), with this data:
   `{"kind": "pubky.music.vault-client", "v": 1, "client_key": "<hex X25519 public key>", "name": "<label>"}`.
   The result, `{grant, pop}`, is the root-signed grant JWS (it names the client key in `cnf`) and the proof JWS.
3. It adds itself to `index.json`.

A reader trusts an entry only if all of these hold:
- `verify_custom_grant_pop` accepts the proof (both signatures, the grant bound to the proof, the grant
  not expired);
- the grant's `iss` is the vault's owner;
- the grant's capabilities cover the vault with `r` and `w`;
- `data.kind` and `data.v` are as above;
- `data.client_key` equals the entry's `client_key`.

The homeserver can't forge an entry. A client re-publishes its proof when its grant is renewed. An entry whose
grant has expired stays listed but gets no new keys.

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
{ "service": "tidal", "label": "gil@example.org", "way": "built-in", "client_id": "…",
  "tokens": { "access_token": "…", "refresh_token": "…", "token_type": "Bearer" },
  "scopes": ["r_usr", "w_usr"], "expires_at": 1761403600, "created_by": "<hex>",
  "last_refresh": { "client": "<hex>", "at": 1761400000 }, "revision": 12, "settings": { "quality": "lossless" } }
```
- `way` says how the sign-in was made: `built-in`, `shared-app`, `own-app`, `imported` or `pasted`. Readers
  that don't know a `way` still use the tokens.
- `revision` goes up by one at every write.
- `settings` holds what should follow the person across clients, such as the quality choice.

## Adding a device, revocation

- **Adding a device:** a client that sees a trusted entry with no recipient in a blob unwraps the data key and
  appends a recipient for it, keeping the epoch, under that blob's lock.
- **Sign out this device:**
  1. The entry moves to `revoked`.
  2. Every blob gets a new data key at `epoch` + 1, is encrypted again, and is wrapped only to the remaining
     trusted clients.
  3. The token is revoked at the service where its API allows that.
  - A removed device can still read what it saw before, nothing after.
- **Sign out everywhere:** delete the blobs, and revoke each token at its service.

Clients remember the highest `epoch` and `revision` they have seen per blob. They don't write over a newer
blob, and they treat an older one as stale and read it again (a homeserver can serve an old copy).

## Refreshing a token

Several clients hold one sign-in, and many services rotate the refresh token, so only one refreshes at a time:

1. A client whose token is about to expire (60 s ahead) or was refused takes the homeserver's write lock on the
   blob: `LOCK /storage/<owner>/priv/pubky.music/vault/<service>.json` with `Timeout: Second-30`, renewed
   every 10 s. This is the pubky SDK's `SessionStorage::lock` on homeservers from 0.16.2.
2. Under the lock it reads the blob again. If another client has already refreshed it (higher `revision`, not
   expired), it uses that and unlocks.
3. Otherwise it refreshes with the refresh token it just read. It writes the new tokens with `revision` + 1 and
   `last_refresh` (`PUT` with the lock's `If` header), then unlocks.
4. A client that finds the blob locked (`423`) waits 1–2 s and reads it again. It never refreshes a token it
   didn't just read under the lock.

**Where `LOCK` is refused** (`405`, an older homeserver), the lease file takes its place:
1. Read `<service>.lease.json` (`{"client": "<hex>", "until": <unix s>}`). If another client's lease is live,
   wait and read the blob again.
2. Otherwise write your own lease (`until` = now + 30), wait 2 s, and read the lease again. Go on only if it is
   still yours.
3. Refresh, write the blob, then delete the lease.

The lease is its own file, not a member of the blob: writing a lease into the blob would race with another
client's token write and could undo a rotated refresh token.

## Limits

Blob ≤ 64 KiB, index ≤ 256 KiB, ≤ 64 clients. Larger answers are refused unread.
