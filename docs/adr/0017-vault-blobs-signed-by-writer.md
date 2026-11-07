# ADR 0017: Vault sign-ins are signed by the device that wrote them

Status: **accepted** (2026-11-07, security review; mn-infra).

## Context

The sources vault (`crates/pm-appdata/VAULT.md`) keeps a person's streaming sign-ins on their own homeserver,
encrypted on the device to every client they authorised. Encryption keeps the homeserver from reading a
sign-in. It doesn't keep anyone from *replacing* one: a writer holding the person's grant, or the homeserver
itself, can put a blob of its own at a sign-in's path, sealed to the real clients' keys, and every client
opens and uses it. The homeserver can also serve an older copy of a blob, or of the device list, and take a
sign-in or a sign-out back (found again by several reviews: "rollback, L-9").

Each client already proves its vault key with a custom proof signed under its Pubky Ring grant. That proof
can't double as a blob signature: the SDK refuses a proof once its grant has expired, and grants are
renewed, so a blob signed that way would stop verifying though nothing about it changed.

## Decision

- **A signing key per client, derived, not stored.** Ed25519, with the seed
  `HKDF-SHA-256(ikm = the client's X25519 vault secret, info = "pubky.music/vault/v2/sign")`. There is no
  second secret to keep, back up or restore.
- **Bound by a second proof, not by a change to the first.** Each index entry gets `signer: {grant, pop}`,
  a custom proof made and verified exactly like the client proof, with data
  `{kind: "pubky.music.vault-signer", v: 1, client_key, sign_key}`. The client proof's data struct denies
  unknown fields and checks its version exactly; changing it would make every older reader distrust updated
  clients. A separate member is kept, untouched, by older readers.
- **Every blob is signed by its writer** (`writer: {client_key, sig}`) over the owner, path, epoch, the
  recipients and the ciphertext (which covers the revision and the plaintext). Every rewrite re-signs.
- **A reader takes a signed blob only from a device trusted today** (a verifying entry, not signed out, with
  a signer proof), and an unsigned blob only while it has seen no signed copy of that service and, in a
  node, has no copy of its own. A blob nobody trusted signed is written over by the node's own copy (a new
  key, the next epoch), never rewritten from its content; signing a device out still rotates what that
  device wrote while trusted, since the rotation runs under the blob's lock right after the list says it
  is out.
- **Rollback guard:** each reader keeps the highest `(epoch, revision)` seen per blob and the highest
  sign-out-everywhere marker, in a local file beside its record of sign-outs. An older signed copy is refused
  as stale; a device list with a lower marker doesn't undo a sign-out everywhere. The marks start over when
  the marker moves, when the reader deletes or finds a blob gone, or when the person takes the vault's copy
  by hand.
- **Migration:** the envelope's `version` stays 1 so older readers keep working; a node signs a blob from
  before v2 once its content equals the node's own copy; the window closes in the release after every
  known vault client signs. No vault client exists outside this repository today (the desktop and Android
  apps are shells around the node's web interface, ADR 0012); one written later follows VAULT.md v2.

## Consequences

- A homeserver, or a writer with a broad grant, can no longer put a sign-in of its own in a node's vault, nor
  serve an old one back. A node keeps its own sign-in instead and says why in Settings.
- A device signed out since can't have anything it wrote taken afterwards, even a blob written before the
  sign-out.
- One more custom proof per registration, one signature per write, one index read per blob read.
- During the migration window an un-updated node in a mixed fleet writes unsigned blobs that updated nodes
  don't take over their own copy; the release note says to update all of a person's nodes together.
- Reference vectors (`tests/vectors/vault-v2.json`) and an independent Python check let another
  implementation verify its derivation and signatures byte for byte.
