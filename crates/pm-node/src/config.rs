//! Node configuration: one TOML file, every section optional with defaults.

use pm_pubky::PmPubkyConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NodeConfig {
    /// Where the SQLite library and other state live.
    pub data_dir: PathBuf,
    /// Contact address for outbound User-Agent strings (MusicBrainz requires one).
    pub contact: String,
    pub library: LibraryConfig,
    /// Pinakes, Nexus and network settings (see `pm_pubky::PmPubkyConfig`).
    pub pubky: PmPubkyConfig,
    pub api: ApiConfig,
    pub outputs: OutputsConfig,
    /// TIDAL developer app (developer.tidal.com); empty = the project's built-in app (sign-in with
    /// PKCE through the return page, no secret).
    pub tidal: TidalConfig,
    /// Qobuz (unofficial): an app ID and secret the owner may use; Sources › Qobuz can set them.
    pub qobuz: QobuzConfig,
    /// Spotify developer app (developer.spotify.com); optional.
    pub spotify: SpotifyConfig,
    /// AcoustID application key for identifying untagged files; optional.
    pub acoustid: AcoustIdConfig,
    /// Jamendo developer app (devportal.jamendo.com); `PM_JAMENDO_CLIENT_ID` and
    /// `PM_JAMENDO_CLIENT_SECRET` override. Empty = the project's built-in app (no Jamendo sign-in).
    pub jamendo: JamendoConfig,
    /// Remote access over iroh (ADR 0013).
    pub remote: RemoteConfig,
    /// YouTube on the zones (unofficial): the yt-dlp to run; empty = one the node downloaded, or
    /// yt-dlp on the PATH. `PM_YTDLP` overrides.
    pub youtube: YoutubeConfig,
    pub soundcloud: SoundcloudConfig,
    /// Sign-ins with the project's built-in apps.
    pub auth: AuthConfig,
    /// Every zone as a UPnP/DLNA MediaRenderer on the LAN (Music Assistant, Home Assistant,
    /// BubbleUPnP can play to it). No token, like any renderer.
    pub renderer: RendererConfig,
    /// The Pubky homeserver bundled with the node (deploy/bundle); `PM_HOMESERVER_ADMIN_URL` and
    /// `PM_HOMESERVER_ADMIN_PASSWORD` override.
    pub homeserver: HomeserverConfig,
}

/// The bundled homeserver's admin API. Empty = no bundled homeserver.
#[derive(Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct HomeserverConfig {
    pub admin_url: String,
    pub admin_password: String,
    /// The folder shared with cloudflared and the config wrapper (`PM_TUNNEL_DIR`); empty = no
    /// tunnel setup from Settings.
    pub tunnel_dir: String,
    /// The bundled homeserver's files folder, mounted read-only (`PM_HOMESERVER_FILES_DIR`): its
    /// music becomes a library folder.
    pub files_dir: String,
}

impl std::fmt::Debug for HomeserverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomeserverConfig")
            .field("admin_url", &self.admin_url)
            .field(
                "admin_password",
                &if self.admin_password.is_empty() {
                    ""
                } else {
                    "<set>"
                },
            )
            .field("tunnel_dir", &self.tunnel_dir)
            .field("files_dir", &self.files_dir)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RendererConfig {
    pub enabled: bool,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct YoutubeConfig {
    pub ytdlp: String,
}

/// SoundCloud (unofficial): a client ID the owner may use and, optionally, their own sign-in
/// token; Settings override both. `PM_SOUNDCLOUD_CLIENT_ID` / `PM_SOUNDCLOUD_TOKEN` as well.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SoundcloudConfig {
    pub client_id: String,
    pub token: String,
}

impl std::fmt::Debug for SoundcloudConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundcloudConfig")
            .field("client_id", &self.client_id)
            .field("token", &if self.token.is_empty() { "" } else { "<set>" })
            .finish()
    }
}

/// Sign-ins with the project's built-in apps return through one static page that forwards the
/// browser to this node (STEWARD-SHARED-APPS.md).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AuthConfig {
    /// The return page; empty = the project's (`pm_api::project_apps::RETURN_PAGE`).
    pub return_page: String,
}

/// Remote access over iroh (ADR 0013): the owner's apps reach the node from anywhere by its key,
/// which only they can read (the private node record). Runs only while the node has a Pubky session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RemoteConfig {
    pub enabled: bool,
    /// Empty = n0's public relays; an `https://` URL = a self-hosted iroh-relay; `none` = direct
    /// connections only.
    pub relay: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            relay: String::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct JamendoConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for JamendoConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JamendoConfig")
            .field("client_id", &if self.client_id.is_empty() { "" } else { "<set>" })
            .field(
                "client_secret",
                &if self.client_secret.is_empty() {
                    ""
                } else {
                    "<set>"
                },
            )
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct AcoustIdConfig {
    /// From acoustid.org/new-application; `PM_ACOUSTID_KEY` overrides.
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub client_secret: String,
    /// A Spotify Soloist executable the owner installed (`PM_SOLOIST` overrides); otherwise the
    /// node's downloaded one in `<data_dir>/bin`, or one on the PATH.
    pub soloist: String,
    /// A librespot executable (`PM_LIBRESPOT` overrides); otherwise `<data_dir>/bin/librespot` or the
    /// PATH. librespot publishes no ready-made builds: `cargo install librespot` or a distro package.
    pub librespot: String,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct QobuzConfig {
    pub app_id: String,
    pub app_secret: String,
}

impl std::fmt::Debug for QobuzConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QobuzConfig")
            .field("app_id", &self.app_id)
            .field(
                "app_secret",
                &if self.app_secret.is_empty() { "" } else { "<set>" },
            )
            .finish()
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TidalConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Space-separated OAuth scopes to request at sign-in; empty = the node's default
    /// (user.read collection.read collection.write playlists.read playlists.write playback
    /// entitlements.read search.read). Every scope must be enabled on the developer app.
    pub scopes: String,
    /// Full tracks (unofficial): a TIDAL device client id and secret for the device sign-in.
    /// Settings › Sources › TIDAL can set or override them.
    pub unofficial_client_id: String,
    pub unofficial_client_secret: String,
}

impl std::fmt::Debug for TidalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |v: &str| if v.is_empty() { "" } else { "<set>" };
        f.debug_struct("TidalConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &set(&self.client_secret))
            .field("scopes", &self.scopes)
            .field("unofficial_client_id", &set(&self.unofficial_client_id))
            .field("unofficial_client_secret", &set(&self.unofficial_client_secret))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LibraryConfig {
    /// Folders to scan. Mounted shares count as folders.
    pub folders: Vec<PathBuf>,
    /// Rescan automatically when files change in the folders (default on).
    pub watch: bool,
    /// Seconds without changes before an automatic rescan starts.
    pub watch_quiet_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ApiConfig {
    pub listen: String,
    /// Bearer token clients must send. Empty = no auth: keep `listen` on localhost then.
    pub token: String,
    /// Name shown to controllers.
    pub name: String,
    /// Username for OpenSubsonic clients; the password is `token`.
    pub subsonic_user: String,
    /// How the user's devices reach this API (`https://music.example.org`, a
    /// Tailscale name, …). Published privately on the homeserver when a session
    /// exists (ADR 0008). Empty = `http://<listen>`.
    pub public_url: String,
    /// Announce the node on the LAN (mDNS `_pubky-music._tcp` and SSDP) so apps
    /// find it. Only when `listen` is not a loopback address.
    pub announce: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            contact: String::new(),
            library: LibraryConfig::default(),
            pubky: PmPubkyConfig::default(),
            api: ApiConfig::default(),
            outputs: OutputsConfig::default(),
            tidal: TidalConfig::default(),
            qobuz: QobuzConfig::default(),
            spotify: SpotifyConfig::default(),
            acoustid: AcoustIdConfig::default(),
            jamendo: JamendoConfig::default(),
            remote: RemoteConfig::default(),
            youtube: YoutubeConfig::default(),
            soundcloud: SoundcloudConfig::default(),
            renderer: RendererConfig::default(),
            homeserver: HomeserverConfig::default(),
            auth: AuthConfig::default(),
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            watch: true,
            watch_quiet_secs: 15,
        }
    }
}

/// Network endpoints (`pm-endpoint`) this node plays to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OutputsConfig {
    /// Endpoint addresses (`host:port`) to connect to at start, besides discovery.
    pub endpoints: Vec<String>,
    /// Browse mDNS for endpoints at start.
    pub discover: bool,
    /// How long to listen for mDNS answers.
    pub discover_ms: u64,
    /// Discover UPnP/OpenHome renderers (SSDP) and offer them as outputs.
    pub upnp: bool,
    /// Slimproto port for Squeezebox / squeezelite players; 0 disables.
    pub slimproto_port: u16,
    /// Discover Chromecast / Google Cast devices (mDNS) and offer them as outputs.
    pub cast: bool,
    /// A snapserver (`host` or `host:port` of its JSON-RPC, port 1705 by default): every Snapcast
    /// group becomes a zone. Empty = off.
    pub snapcast: String,
    /// Discover AirPlay 1 receivers (mDNS `_raop._tcp`) and offer them as outputs.
    pub airplay: bool,
}

impl Default for OutputsConfig {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            discover: true,
            discover_ms: 1500,
            upnp: true,
            slimproto_port: 3483,
            cast: true,
            snapcast: String::new(),
            airplay: true,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7788".into(),
            token: String::new(),
            name: "Pubky Music Node".into(),
            subsonic_user: "pubky".into(),
            public_url: String::new(),
            announce: true,
        }
    }
}

impl NodeConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn load_or_default(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) if p.exists() => Self::load(p),
            Some(p) => anyhow::bail!("config file {} not found", p.display()),
            None => Ok(Self::default()),
        }
    }

    /// After a restore the old node's config sits in `<data_dir>/node.restored.toml`: its service
    /// keys (TIDAL, Spotify, Jamendo, AcoustID) and contact fill what this config leaves empty.
    /// Returns what was taken.
    pub fn fill_from_restored(&mut self) -> Vec<&'static str> {
        let Ok(text) = std::fs::read_to_string(self.data_dir.join("node.restored.toml")) else {
            return Vec::new();
        };
        let Ok(old) = toml::from_str::<NodeConfig>(&text) else {
            return Vec::new();
        };
        let mut took = Vec::new();
        if self.tidal.client_id.is_empty() && !old.tidal.client_id.is_empty() {
            self.tidal = old.tidal;
            took.push("tidal");
        }
        if self.spotify.client_id.is_empty() && !old.spotify.client_id.is_empty() {
            self.spotify = old.spotify;
            took.push("spotify");
        }
        if self.jamendo.client_id.is_empty() && !old.jamendo.client_id.is_empty() {
            self.jamendo = old.jamendo;
            took.push("jamendo");
        }
        if self.acoustid.api_key.is_empty() && !old.acoustid.api_key.is_empty() {
            self.acoustid = old.acoustid;
            took.push("acoustid");
        }
        if self.contact.is_empty() && !old.contact.is_empty() {
            self.contact = old.contact;
            took.push("contact");
        }
        took
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }

    /// The pm-pubky config with the node-level contact filled in. Errors
    /// when no usable contact is set, since every upstream requires one.
    pub fn pubky_config(&self) -> anyhow::Result<PmPubkyConfig> {
        let mut c = self.pubky.clone();
        if !self.contact.is_empty() {
            c.user_agent_contact = self.contact.clone();
        }
        c.validate_contact()
            .map_err(|e| anyhow::anyhow!("{e}: set `contact` in the node config"))?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restored_config_fills_only_empty_service_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("node.restored.toml"),
            "contact = \"old@example.org\"\n[tidal]\nclient_id = \"old-id\"\nclient_secret = \"old-secret\"\n[spotify]\nclient_id = \"sp\"\n",
        )
        .unwrap();
        let mut cfg = NodeConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        cfg.spotify.client_id = "mine".into();
        let took = cfg.fill_from_restored();
        assert_eq!(took, vec!["tidal", "contact"]);
        assert_eq!(cfg.tidal.client_secret, "old-secret");
        assert_eq!(cfg.spotify.client_id, "mine", "this node's own key wins");
    }

    #[test]
    fn parses_partial_toml() {
        let cfg: NodeConfig = toml::from_str(
            r#"
contact = "ops@example.org"
[library]
folders = ["/music", "/mnt/nas/flac"]
[api]
listen = "0.0.0.0:7788"
"#,
        )
        .unwrap();
        assert_eq!(cfg.library.folders.len(), 2);
        assert_eq!(cfg.api.listen, "0.0.0.0:7788");
        assert_eq!(cfg.pubky.network, pm_pubky::Network::Mainnet);
        assert!(cfg
            .pubky_config()
            .unwrap()
            .user_agent()
            .contains("ops@example.org"));
        assert!(NodeConfig::default().pubky_config().is_err());
    }

    #[test]
    fn default_roundtrip() {
        let cfg = NodeConfig::default();
        let text = toml::to_string(&cfg).unwrap();
        let back: NodeConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }
}
