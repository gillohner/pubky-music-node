# Qobuz

The node plays full tracks from Qobuz, up to 24-bit/192 kHz, through the private API Qobuz's own
players use. Qobuz has not approved this for other apps: it can stop working at any time, and it
uses your Qobuz account. It needs a paid Qobuz streaming plan.

Everything is on one page: **Sources › Qobuz**. The status line at the top says where you are
(Off, Needs app, Needs sign-in, Working) and the two steps below it do the rest.

## What you need

1. **A Qobuz app ID and secret.** Qobuz issues these to partners; there is no public developer
   page. Ask Qobuz for API access at [api@qobuz.com](mailto:api@qobuz.com) and say what it is for.
   The node has no built-in app and never takes one from another program's code: the app must be
   one you are allowed to use.
2. **Your Qobuz account** with a streaming plan (Studio or Sublime; Qobuz's download-only plans
   don't stream).

## Step 1: App ID and secret

Paste the app ID (digits) and the secret into the two fields. You can paste a whole
`app_id=… app_secret=…` block, a config section or JSON into either field: it is split into the
two. **Show** reveals a secret while you type it.

**Check** asks Qobuz whether it takes the app, without saving anything and without your account:

| It says | It means |
| :-- | :-- |
| OK: Qobuz accepts this app ID; the secret is checked at the first play | go on to step 2. Qobuz only checks the secret once it plays something for your account: a wrong one then shows as a Problem saying so |
| Qobuz doesn't know this app ID | a typo, or the app was withdrawn; ask Qobuz |
| Qobuz knows this app ID, but the secret doesn't go with it | the secret is wrong |
| Couldn't reach Qobuz | the node has no way to Qobuz right now; try again later |

**Save** keeps the app on the node like a sign-in. The secret is never shown again; the app ID is.
An app set in the node's config (`[qobuz] app_id` and `app_secret`, or `PM_QOBUZ_APP_ID` and
`PM_QOBUZ_APP_SECRET`) is used until you save one here.

**Already use streamrip or qobuz-dl?** Open *Use its config* under step 1: pick its config file,
or paste its contents. The node reads the app values and the sign-in from it and does both steps
at once. The file is only read.

## Step 2: Your Qobuz account

Enter your Qobuz email and password. They go to Qobuz once, to sign in; the node keeps only the
session token Qobuz answers with. Two refusals are said in place:

- *This Qobuz account has no streaming plan*: full tracks need a paid plan; the account signed in,
  but Qobuz won't stream to it.
- *Qobuz doesn't know that email and password*: check them and try again.

Signed in, the status says **Working** with the quality your plan allows, and a **Quality**
setting lets you cap it (Max, CD, or MP3 320 kbps). **Sign out** forgets the session; the app
stays.

## When it stops working

If Qobuz refuses the app or ends the session, the node switches Qobuz off and the status says
**Problem** with what happened; **Try again** switches it back on. Qobuz links keep opening on
qobuz.com meanwhile. If the app was withdrawn, you need a new one from Qobuz.

## What the node keeps

The app ID and secret, and the session token, in the node's database, treated like a sign-in: a
backup carries them only under your passphrase. The password is never stored or logged.
