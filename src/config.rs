//! On-disk state: where it lives, the endpoint secret key, and the peer list.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use iroh::{EndpointAddr, EndpointId, SecretKey, TransportAddr};
use serde::{Deserialize, Serialize};

use crate::util::{from_hex, to_hex};

/// Environment variable that overrides the state directory. Mainly for tests and for
/// running two independent identities on one machine.
pub const HOME_ENV: &str = "AGENT2AGENT_HOME";

/// Resolved locations of everything we persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    dir: PathBuf,
}

impl Paths {
    /// Use an explicit directory.
    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `$AGENT2AGENT_HOME`, else `$XDG_CONFIG_HOME/agent2agent`, else `~/.config/agent2agent`.
    pub fn resolve() -> Result<Self> {
        if let Some(dir) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
            return Ok(Self::from_dir(dir));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self::from_dir(PathBuf::from(xdg).join("agent2agent")));
        }
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).context(
            "cannot determine the state directory: neither AGENT2AGENT_HOME nor HOME is set",
        )?;
        Ok(Self::from_dir(
            PathBuf::from(home).join(".config").join("agent2agent"),
        ))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn secret_key(&self) -> PathBuf {
        self.dir.join("secret.key")
    }

    pub fn peers(&self) -> PathBuf {
        self.dir.join("peers.toml")
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("daemon.sock")
    }

    pub fn identity(&self) -> PathBuf {
        self.dir.join("identity.toml")
    }

    /// Create the state directory if needed, owner-accessible only.
    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating state directory {}", self.dir.display()))?;
        set_mode(&self.dir, 0o700)?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode {mode:o} on {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Load the endpoint secret key, generating and persisting one on first run.
///
/// The key is this node's identity: its public half is the [`EndpointId`] peers dial.
/// Losing it means re-pairing; leaking it means someone can impersonate this agent.
pub fn load_or_create_secret_key(paths: &Paths) -> Result<SecretKey> {
    let path = paths.secret_key();
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading secret key {}", path.display()))?;
        let bytes = from_hex(&text)
            .with_context(|| format!("secret key {} is not valid hex", path.display()))?;
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "secret key {} is {} bytes, expected 32",
                path.display(),
                bytes.len()
            )
        })?;
        return Ok(SecretKey::from_bytes(&bytes));
    }

    paths.ensure_dir()?;
    let key = SecretKey::generate();
    let mut text = to_hex(&key.to_bytes());
    text.push('\n');
    std::fs::write(&path, text)
        .with_context(|| format!("writing secret key {}", path.display()))?;
    set_mode(&path, 0o600)?;
    Ok(key)
}

/// One entry in the peer list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// The peer's [`EndpointId`], as printed by `agent2agent id`.
    pub id: String,
    /// Optional pinned direct addresses (`ip:port`).
    ///
    /// Normally empty: iroh resolves an id to an address on its own. Set these to talk
    /// over a LAN with no internet, or to avoid discovery entirely.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
}

/// Whether the operator is in the loop for every message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The agents talk on their own. Nothing is held for approval.
    #[default]
    Auto,
    /// Every message, in either direction, is shown to the operator and waits for a
    /// yes before it moves.
    Manual,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Manual => "manual",
        }
    }

    pub fn is_manual(self) -> bool {
        matches!(self, Mode::Manual)
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Mode::Auto),
            "manual" => Ok(Mode::Manual),
            other => bail!("unknown mode {other:?}, expected 'auto' or 'manual'"),
        }
    }
}

/// What this agent calls itself, remembered per working directory.
///
/// An agent has no durable sense of its own name, so without this it would pick a new one
/// every session and the other side would watch a stranger arrive each time. Keyed by
/// directory because that is what a session is anchored to: the same project gets the
/// same name back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Used when the current directory has no name of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Absolute directory path to chosen name.
    #[serde(default)]
    pub dirs: BTreeMap<String, String>,
}

impl Identity {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing identity")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The name to use in `dir`, falling back to the default.
    pub fn name_for(&self, dir: &Path) -> Option<String> {
        self.dirs
            .get(&dir.to_string_lossy().to_string())
            .cloned()
            .or_else(|| self.default.clone())
    }

    /// Remember `name` for `dir`, and as the fallback if there is none yet.
    pub fn remember(&mut self, dir: &Path, name: &str) -> Result<()> {
        validate_name(name)?;
        self.dirs
            .insert(dir.to_string_lossy().to_string(), name.to_string());
        if self.default.is_none() {
            self.default = Some(name.to_string());
        }
        Ok(())
    }
}

/// The contents of `peers.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peers {
    /// Peer used when `--to`/`--from` is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Whether messages wait for operator approval.
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub peers: BTreeMap<String, Peer>,
}

impl Peers {
    /// Read the peer list. A missing file is an empty list, not an error.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write the peer list, creating the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing peer list")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        set_mode(path, 0o600)?;
        Ok(())
    }

    /// Add or replace a peer. The first peer added also becomes the default.
    pub fn add(&mut self, name: &str, id: &str) -> Result<()> {
        validate_name(name)?;
        let parsed = parse_endpoint_id(id)?;
        if let Some((existing, _)) = self
            .peers
            .iter()
            .find(|(other, peer)| other.as_str() != name && peer.id == parsed.to_string())
        {
            bail!("that id is already registered as peer {existing:?}");
        }
        self.peers.insert(
            name.to_string(),
            Peer {
                id: parsed.to_string(),
                addrs: Vec::new(),
            },
        );
        if self.default.is_none() {
            self.default = Some(name.to_string());
        }
        Ok(())
    }

    /// Record a peer discovered through pairing, returning the name actually used.
    ///
    /// Unlike [`Self::add`] this never fails on a name clash: the operator is not typing
    /// the name, the remote side chose it, so a collision with an unrelated existing peer
    /// must not abort a pairing that is already half-complete. A taken name gets a
    /// numeric suffix instead. Re-pairing with a peer already on file reuses its name.
    pub fn add_paired(&mut self, preferred: &str, id: &EndpointId) -> Result<String> {
        validate_name(preferred)?;
        let id_text = id.to_string();

        if let Some(existing) = self.name_for(id) {
            self.peers.insert(
                existing.clone(),
                Peer {
                    id: id_text,
                    addrs: self
                        .peers
                        .get(&existing)
                        .map(|p| p.addrs.clone())
                        .unwrap_or_default(),
                },
            );
            return Ok(existing);
        }

        let mut name = preferred.to_string();
        let mut suffix = 2;
        while self.peers.contains_key(&name) {
            name = format!("{preferred}-{suffix}");
            suffix += 1;
        }

        self.peers.insert(
            name.clone(),
            Peer {
                id: id_text,
                addrs: Vec::new(),
            },
        );
        if self.default.is_none() {
            self.default = Some(name.clone());
        }
        Ok(name)
    }

    /// Remove a peer. Returns whether it existed. Clears the default if it pointed here.
    pub fn remove(&mut self, name: &str) -> bool {
        let existed = self.peers.remove(name).is_some();
        if self.default.as_deref() == Some(name) {
            // Fall back to any remaining peer so the no-flag path keeps working.
            self.default = self.peers.keys().next().cloned();
        }
        existed
    }

    /// Set the default peer. It must already be in the list.
    pub fn set_default(&mut self, name: &str) -> Result<()> {
        if !self.peers.contains_key(name) {
            bail!("unknown peer {name:?}");
        }
        self.default = Some(name.to_string());
        Ok(())
    }

    /// Resolve a possibly-absent peer name to a concrete peer.
    ///
    /// With no name given: the default peer, or the only peer if there is exactly one.
    pub fn resolve(&self, name: Option<&str>) -> Result<(String, &Peer)> {
        let chosen = match name {
            Some(n) => n.to_string(),
            None => match &self.default {
                Some(d) => d.clone(),
                None => match self.peers.len() {
                    0 => bail!("no peers configured; add one with `agent2agent peer add <name> <id>`"),
                    1 => self.peers.keys().next().expect("len checked").clone(),
                    _ => bail!(
                        "several peers configured and no default set; pass --to <name> or run `agent2agent peer default <name>`"
                    ),
                },
            },
        };
        let peer = self
            .peers
            .get(&chosen)
            .ok_or_else(|| anyhow::anyhow!("unknown peer {chosen:?}"))?;
        Ok((chosen, peer))
    }

    /// The name we know an id by, if any. This is the authorization check: a connection
    /// from an id that maps to no name is refused.
    pub fn name_for(&self, id: &EndpointId) -> Option<String> {
        let wanted = id.to_string();
        self.peers
            .iter()
            .find(|(_, peer)| peer.id == wanted)
            .map(|(name, _)| name.clone())
    }
}

impl Peer {
    /// Parse the stored id.
    pub fn endpoint_id(&self) -> Result<EndpointId> {
        parse_endpoint_id(&self.id)
    }

    /// Build the address to dial: the id, plus any pinned direct addresses.
    pub fn endpoint_addr(&self) -> Result<EndpointAddr> {
        let id = self.endpoint_id()?;
        let mut addrs = Vec::new();
        for raw in &self.addrs {
            let parsed: SocketAddr = raw
                .parse()
                .with_context(|| format!("peer address {raw:?} is not a valid ip:port"))?;
            addrs.push(TransportAddr::Ip(parsed));
        }
        Ok(EndpointAddr::from_parts(id, addrs))
    }
}

/// Parse an [`EndpointId`] as printed by `agent2agent id`.
pub fn parse_endpoint_id(s: &str) -> Result<EndpointId> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty endpoint id");
    }
    s.parse::<EndpointId>()
        .map_err(|e| anyhow::anyhow!("{s:?} is not a valid endpoint id: {e}"))
}

/// Peer names go into file paths and command lines, so keep them boring.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("peer name cannot be empty");
    }
    if name.len() > 64 {
        bail!("peer name is too long (max 64 characters)");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("peer name {name:?} may only contain letters, digits, '-' and '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn some_id() -> String {
        SecretKey::generate().public().to_string()
    }

    #[test]
    fn endpoint_id_survives_a_display_parse_roundtrip() {
        // The whole pairing UX is "copy this string to the other machine", so the
        // printed form must parse back into the same key.
        let public = SecretKey::generate().public();
        let printed = public.to_string();
        let parsed = parse_endpoint_id(&printed).unwrap();
        assert_eq!(parsed, public);
    }

    #[test]
    fn parse_endpoint_id_rejects_junk() {
        assert!(parse_endpoint_id("").is_err());
        assert!(parse_endpoint_id("   ").is_err());
        assert!(parse_endpoint_id("not-a-key").is_err());
    }

    #[test]
    fn paths_prefer_the_home_env_var() {
        let dir = TempDir::new().unwrap();
        temp_env(HOME_ENV, dir.path().to_str().unwrap(), || {
            let paths = Paths::resolve().unwrap();
            assert_eq!(paths.dir(), dir.path());
            assert_eq!(paths.secret_key().file_name().unwrap(), "secret.key");
            assert_eq!(paths.peers().file_name().unwrap(), "peers.toml");
            assert_eq!(paths.socket().file_name().unwrap(), "daemon.sock");
        });
    }

    #[test]
    fn secret_key_is_generated_once_and_then_reused() {
        let dir = TempDir::new().unwrap();
        let paths = Paths::from_dir(dir.path());

        let first = load_or_create_secret_key(&paths).unwrap();
        assert!(paths.secret_key().exists());
        let second = load_or_create_secret_key(&paths).unwrap();

        assert_eq!(first.public(), second.public(), "identity must be stable");
    }

    #[cfg(unix)]
    #[test]
    fn secret_key_and_dir_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let paths = Paths::from_dir(dir.path().join("state"));
        load_or_create_secret_key(&paths).unwrap();

        let key_mode = std::fs::metadata(paths.secret_key())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(key_mode, 0o600, "secret key mode was {key_mode:o}");

        let dir_mode = std::fs::metadata(paths.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "state dir mode was {dir_mode:o}");
    }

    #[test]
    fn corrupt_secret_key_is_reported_not_silently_replaced() {
        let dir = TempDir::new().unwrap();
        let paths = Paths::from_dir(dir.path());
        paths.ensure_dir().unwrap();
        std::fs::write(paths.secret_key(), "not hex at all").unwrap();
        assert!(load_or_create_secret_key(&paths).is_err());

        // Right length as hex, wrong length as bytes.
        std::fs::write(paths.secret_key(), "abcd").unwrap();
        assert!(load_or_create_secret_key(&paths).is_err());
    }

    #[test]
    fn peers_roundtrip_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("peers.toml");

        let mut peers = Peers::default();
        peers.add("codex", &some_id()).unwrap();
        peers.add("gemini", &some_id()).unwrap();
        peers.save(&path).unwrap();

        let loaded = Peers::load(&path).unwrap();
        assert_eq!(loaded, peers);
    }

    #[test]
    fn missing_peers_file_loads_as_empty() {
        let dir = TempDir::new().unwrap();
        let loaded = Peers::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(loaded, Peers::default());
    }

    #[test]
    fn malformed_peers_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("peers.toml");
        std::fs::write(&path, "this is not [ valid toml").unwrap();
        assert!(Peers::load(&path).is_err());
    }

    #[test]
    fn first_peer_added_becomes_the_default() {
        let mut peers = Peers::default();
        peers.add("codex", &some_id()).unwrap();
        assert_eq!(peers.default.as_deref(), Some("codex"));

        peers.add("gemini", &some_id()).unwrap();
        assert_eq!(
            peers.default.as_deref(),
            Some("codex"),
            "default is not stolen"
        );
    }

    #[test]
    fn add_rejects_bad_names_and_bad_ids() {
        let mut peers = Peers::default();
        assert!(peers.add("", &some_id()).is_err());
        assert!(peers.add("has space", &some_id()).is_err());
        assert!(peers.add("has/slash", &some_id()).is_err());
        assert!(peers.add(&"x".repeat(65), &some_id()).is_err());
        assert!(peers.add("ok", "garbage").is_err());
        assert!(peers.peers.is_empty(), "nothing was added");
    }

    #[test]
    fn add_rejects_the_same_id_under_a_second_name() {
        let id = some_id();
        let mut peers = Peers::default();
        peers.add("codex", &id).unwrap();
        let err = peers.add("codex-again", &id).unwrap_err();
        assert!(err.to_string().contains("codex"), "unexpected error: {err}");
    }

    #[test]
    fn add_can_update_an_existing_name_in_place() {
        let mut peers = Peers::default();
        peers.add("codex", &some_id()).unwrap();
        let new_id = some_id();
        peers.add("codex", &new_id).unwrap();
        assert_eq!(peers.peers["codex"].id, new_id);
        assert_eq!(peers.peers.len(), 1);
    }

    #[test]
    fn identity_remembers_a_name_per_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("identity.toml");

        let mut identity = Identity::default();
        assert_eq!(identity.name_for(Path::new("/work/a")), None);

        identity.remember(Path::new("/work/a"), "clod").unwrap();
        identity.remember(Path::new("/work/b"), "prof").unwrap();
        identity.save(&path).unwrap();

        let loaded = Identity::load(&path).unwrap();
        assert_eq!(
            loaded.name_for(Path::new("/work/a")).as_deref(),
            Some("clod")
        );
        assert_eq!(
            loaded.name_for(Path::new("/work/b")).as_deref(),
            Some("prof")
        );
    }

    #[test]
    fn identity_falls_back_to_the_first_name_taken() {
        // A directory never seen before still gets a name, so the agent is recognisable
        // rather than anonymous.
        let mut identity = Identity::default();
        identity.remember(Path::new("/work/a"), "clod").unwrap();

        assert_eq!(
            identity.name_for(Path::new("/somewhere/else")).as_deref(),
            Some("clod")
        );
        assert_eq!(identity.default.as_deref(), Some("clod"));
    }

    #[test]
    fn identity_keeps_the_same_name_on_repeat_visits() {
        // The whole point: the agent must not invent a new name next session.
        let mut identity = Identity::default();
        identity.remember(Path::new("/work/a"), "clod").unwrap();
        let first = identity.name_for(Path::new("/work/a"));
        let second = identity.name_for(Path::new("/work/a"));
        assert_eq!(first, second);
        assert_eq!(first.as_deref(), Some("clod"));
    }

    #[test]
    fn identity_can_be_renamed_for_a_directory() {
        let mut identity = Identity::default();
        identity.remember(Path::new("/work/a"), "clod").unwrap();
        identity.remember(Path::new("/work/a"), "mia").unwrap();
        assert_eq!(
            identity.name_for(Path::new("/work/a")).as_deref(),
            Some("mia")
        );
        assert_eq!(identity.dirs.len(), 1);
    }

    #[test]
    fn identity_rejects_an_unusable_name() {
        let mut identity = Identity::default();
        assert!(identity
            .remember(Path::new("/work/a"), "has space")
            .is_err());
        assert!(identity.remember(Path::new("/work/a"), "").is_err());
        assert!(identity.dirs.is_empty());
    }

    #[test]
    fn a_missing_identity_file_is_empty_not_an_error() {
        let dir = TempDir::new().unwrap();
        let loaded = Identity::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(loaded, Identity::default());
    }

    #[test]
    fn mode_defaults_to_auto_and_survives_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("peers.toml");

        let mut peers = Peers::default();
        assert_eq!(
            peers.mode,
            Mode::Auto,
            "agents talk freely unless told not to"
        );

        peers.mode = Mode::Manual;
        peers.save(&path).unwrap();
        assert_eq!(Peers::load(&path).unwrap().mode, Mode::Manual);
    }

    #[test]
    fn a_config_written_before_modes_existed_still_loads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("peers.toml");
        std::fs::write(&path, "default = \"codex\"\n\n[peers.codex]\nid = \"x\"\n").unwrap();

        let loaded = Peers::load(&path).unwrap();
        assert_eq!(loaded.mode, Mode::Auto);
    }

    #[test]
    fn mode_parses_from_text_case_insensitively() {
        assert_eq!("auto".parse::<Mode>().unwrap(), Mode::Auto);
        assert_eq!("MANUAL".parse::<Mode>().unwrap(), Mode::Manual);
        assert_eq!(" manual \n".parse::<Mode>().unwrap(), Mode::Manual);
        assert!("halfway".parse::<Mode>().is_err());
        assert!("".parse::<Mode>().is_err());
    }

    #[test]
    fn mode_round_trips_through_its_display_form() {
        for mode in [Mode::Auto, Mode::Manual] {
            assert_eq!(mode.to_string().parse::<Mode>().unwrap(), mode);
        }
        assert!(Mode::Manual.is_manual());
        assert!(!Mode::Auto.is_manual());
    }

    #[test]
    fn add_paired_uses_the_requested_name_when_it_is_free() {
        let id = SecretKey::generate().public();
        let mut peers = Peers::default();

        let name = peers.add_paired("claude", &id).unwrap();
        assert_eq!(name, "claude");
        assert_eq!(peers.default.as_deref(), Some("claude"));
        assert_eq!(peers.name_for(&id).as_deref(), Some("claude"));
    }

    #[test]
    fn add_paired_suffixes_around_an_unrelated_name_clash() {
        // Pairing must not fail just because the operator already has a peer by that
        // name — the remote side picked it, and the handshake is already half done.
        let mut peers = Peers::default();
        peers.add("claude", &some_id()).unwrap();

        let newcomer = SecretKey::generate().public();
        let name = peers.add_paired("claude", &newcomer).unwrap();
        assert_eq!(name, "claude-2");
        assert_eq!(peers.peers.len(), 2);
        assert_eq!(peers.name_for(&newcomer).as_deref(), Some("claude-2"));

        let third = SecretKey::generate().public();
        assert_eq!(peers.add_paired("claude", &third).unwrap(), "claude-3");
    }

    #[test]
    fn add_paired_reuses_the_name_of_a_peer_already_on_file() {
        let id = SecretKey::generate().public();
        let mut peers = Peers::default();
        peers.add("codex", &id.to_string()).unwrap();

        // Re-pairing with the same key must not create a duplicate under a new name.
        let name = peers.add_paired("something-else", &id).unwrap();
        assert_eq!(name, "codex");
        assert_eq!(peers.peers.len(), 1);
    }

    #[test]
    fn add_paired_rejects_an_unusable_name() {
        let id = SecretKey::generate().public();
        let mut peers = Peers::default();
        assert!(peers.add_paired("has space", &id).is_err());
        assert!(peers.peers.is_empty());
    }

    #[test]
    fn remove_reports_existence_and_repoints_the_default() {
        let mut peers = Peers::default();
        peers.add("codex", &some_id()).unwrap();
        peers.add("gemini", &some_id()).unwrap();
        assert_eq!(peers.default.as_deref(), Some("codex"));

        assert!(peers.remove("codex"));
        assert_eq!(
            peers.default.as_deref(),
            Some("gemini"),
            "default follows to a surviving peer"
        );

        assert!(peers.remove("gemini"));
        assert_eq!(peers.default, None);
        assert!(!peers.remove("gemini"), "second removal reports absence");
    }

    #[test]
    fn set_default_requires_a_known_peer() {
        let mut peers = Peers::default();
        peers.add("codex", &some_id()).unwrap();
        assert!(peers.set_default("codex").is_ok());
        assert!(peers.set_default("nobody").is_err());
    }

    #[test]
    fn resolve_uses_the_default_then_the_only_peer() {
        let mut peers = Peers::default();
        assert!(peers.resolve(None).is_err(), "no peers at all");

        peers.add("codex", &some_id()).unwrap();
        assert_eq!(peers.resolve(None).unwrap().0, "codex");
        assert_eq!(peers.resolve(Some("codex")).unwrap().0, "codex");
        assert!(peers.resolve(Some("nobody")).is_err());

        // Two peers with no default is ambiguous and must not guess.
        peers.add("gemini", &some_id()).unwrap();
        peers.default = None;
        let err = peers.resolve(None).unwrap_err();
        assert!(err.to_string().contains("--to"), "unexpected error: {err}");

        peers.set_default("gemini").unwrap();
        assert_eq!(peers.resolve(None).unwrap().0, "gemini");
    }

    #[test]
    fn name_for_is_the_authorization_check() {
        let known = SecretKey::generate().public();
        let stranger = SecretKey::generate().public();

        let mut peers = Peers::default();
        peers.add("codex", &known.to_string()).unwrap();

        assert_eq!(peers.name_for(&known).as_deref(), Some("codex"));
        assert_eq!(
            peers.name_for(&stranger),
            None,
            "strangers are not authorized"
        );
    }

    #[test]
    fn endpoint_addr_carries_pinned_addresses() {
        let id = some_id();
        let peer = Peer {
            id: id.clone(),
            addrs: vec!["127.0.0.1:4242".into(), "10.0.0.1:9000".into()],
        };
        let addr = peer.endpoint_addr().unwrap();
        assert_eq!(addr.id.to_string(), id);
        let ips: Vec<String> = addr.ip_addrs().map(|a| a.to_string()).collect();
        assert!(ips.contains(&"127.0.0.1:4242".to_string()));
        assert!(ips.contains(&"10.0.0.1:9000".to_string()));
    }

    #[test]
    fn endpoint_addr_without_pins_is_id_only() {
        let peer = Peer {
            id: some_id(),
            addrs: vec![],
        };
        let addr = peer.endpoint_addr().unwrap();
        assert_eq!(addr.ip_addrs().count(), 0);
    }

    #[test]
    fn endpoint_addr_rejects_a_malformed_pin() {
        let peer = Peer {
            id: some_id(),
            addrs: vec!["not-an-address".into()],
        };
        assert!(peer.endpoint_addr().is_err());
    }

    /// Set an env var for the duration of `f`. Tests touching the environment are
    /// serialized by a mutex, since the environment is process-global.
    fn temp_env(key: &str, value: &str, f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match previous {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
