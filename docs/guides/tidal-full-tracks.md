# TIDAL full tracks

TIDAL has two sign-ins on this node, side by side in **Sources › TIDAL › Sign in**:

- **The official app** (the project's, or your own from TIDAL's developer site): search, albums, your
  collection and playlist imports. TIDAL plays **30-second previews** to apps on its third-party
  developer tier, so this one doesn't give you full tracks.
- **The device sign-in** (full tracks, unofficial): the way TIDAL's own devices and programs like
  tidal-dl-ng sign in, with a code you enter at link.tidal.com. TIDAL has not approved this for
  other apps: it can stop working at any time, and it uses your TIDAL account. It needs a paid
  TIDAL plan. This guide is about this one.

The **Full tracks** status at the top of the Sign in group says where you are: Needs client, Needs
sign-in, Waiting (for the code), Working, Off, or Problem.

## Step 1: a device client (only when the build has none)

The device sign-in needs a TIDAL **device client** ID and secret. Some builds of the node carry
one; when yours doesn't, the page shows a step card for it. Enter a device client you are allowed
to use. The node never takes one from another program and never ships one it isn't allowed to.

An app from TIDAL's developer site is **not** a device client: TIDAL refuses it for the code sign-in.
That one goes in **Settings › This node › Service apps** and becomes your official app above.

You can paste a whole `client_id=… client_secret=…` block or JSON into either field: it is split.
**Check** asks TIDAL, without saving anything and without your account:

| It says | It means |
| :-- | :-- |
| OK: TIDAL accepts this device client | go on |
| This client can't use the code sign-in | an app from the developer portal, not a device client |
| TIDAL doesn't know this client ID | a typo, or the client was withdrawn |
| TIDAL knows this client ID, but the secret doesn't go with it | the secret is wrong |

**Save and sign in with a code** keeps the client like a sign-in (the secret is never shown again)
and starts the sign-in. The config keys are `[tidal] unofficial_client_id` and
`unofficial_client_secret` (or `PM_TIDAL_UNOFFICIAL_CLIENT_ID` and `_SECRET`); a client saved on
the page is used instead.

## Step 2: the code

The page shows a code, big, and **Open link.tidal.com**: on a phone or a computer that is signed
in to TIDAL, that button opens the page with the code filled in; confirm it there. If the code
isn't filled in, type it, or use **Copy**. The code is good for about five minutes; the time left
is under the button. No password goes through this node.

The page notices when TIDAL says yes and continues by itself: the status turns **Working** and
TIDAL plays full tracks, up to 24-bit/192 kHz (the **Quality** setting can cap it at 16-bit/44.1
kHz). If the code runs out first, the page says so and offers a new one.

Collection and playlist imports still go through the official app: the page offers to sign in with
it too.

## When it stops working

If TIDAL refuses the client or ends the session, the node switches full tracks off, the status
says **Problem** with what happened, and TIDAL plays previews until it works again. **Try again**
switches it back on; **Sign out** forgets the session. A session imported from another app's file
(tidal-dl-ng, a pasted token) can be replaced by a newer file the same way.

## What the node keeps

The device client, and the session tokens, in the node's database, treated like a sign-in: a
backup carries them only under your passphrase.
