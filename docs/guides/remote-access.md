# Remote access

How to reach your node away from home. Pick the first one that fits.

| Way | For | What you set up |
| :-- | :-- | :-- |
| The Pubky Music app | the app on your laptop or phone | sign the node in to Pubky once |
| Tailscale | the app, a browser and Subsonic apps | Tailscale on the node and on each device |
| Cloudflare Tunnel | a browser anywhere, at your own web address | a free Cloudflare account and a domain |

Don't forward port 7788 on your router to the internet. The node expects a private network or a
tunnel with HTTPS in front of it.

## The Pubky Music app (nothing to open)

1. On the node, sign in to Pubky: **Settings › My accounts › Sign in with Pubky**, and approve
   with the Pubky Ring app.
2. In the Pubky Music app, choose **Sign in with Pubky Ring** on the connect page, with the same
   Pubky account.

The node keeps its connection details in your Pubky account, privately. Away from home the app
connects to the node through an encrypted peer-to-peer link (iroh), directly or through a relay.
No port forwarding, no domain, and it works behind Docker and most routers. The node only needs
to reach the internet; it opens nothing on your router.

A browser can't use this link; for a browser away from home, use one of the other two ways.

To turn this off, add these two lines to the node's config file (`/data/node.toml` in Docker,
`/etc/pubky-music/node.toml` on Linux, as for [Snapcast](speakers.md#snapcast-and-mpd)) and
restart the node:

```toml
[remote]
enabled = false
```

## Tailscale

[Tailscale](https://tailscale.com) puts your devices on a private network of their own.

1. Install Tailscale on the machine that runs the node (for Docker: on the host itself) and sign
   in.
2. Install it on your phone or laptop with the same account.
3. Use the node's Tailscale address in the browser, the app or a Subsonic app:
   `http://100.x.y.z:7788` (shown in the Tailscale app), with the same token.

Any similar private network (WireGuard, ZeroTier) works the same way.

## Cloudflare Tunnel (the bundle)

With the [bundle with its own homeserver](install-docker.md#with-your-own-pubky-homeserver),
the node sets up a Cloudflare Tunnel for you. You need a free Cloudflare account and a domain
managed by Cloudflare.

1. In Cloudflare: **Zero Trust › Networks › Tunnels**, create a tunnel and copy its token.
   Under the tunnel's public hostnames, add two:
   - one for the music, for example `music.example.com`, to `http://localhost:7788`;
   - one for the homeserver, for example `pubky.example.com`, to `http://localhost:6286`.
2. In the node: **Settings › Domain and tunnel** (under **This homeserver · admin**), paste the
   token and the two hostnames, and save.
3. Restart once, as Settings tells you: run these in the `deploy/bundle` folder:

   ```sh
   docker compose --profile tunnel up -d
   docker compose up -d --force-recreate homeserver-config homeserver
   ```

The same Settings page then checks both hostnames. Open `https://music.example.com` from
anywhere and sign in with the same token.

With plain Docker or Linux, you can run `cloudflared` or a reverse proxy with HTTPS (Caddy,
nginx) yourself in front of port 7788. The token stays required; never put the node on the
internet without HTTPS.
