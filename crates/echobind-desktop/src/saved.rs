//! Hosts a viewer saved under a friendly name, kept as JSON in the
//! platform's per-user configuration directory.

use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::warn;

const FILE_NAME: &str = "saved-sessions.json";
const FORMAT_VERSION: u32 = 1;
pub const NAME_LIMIT: usize = 48;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSession {
    pub name: String,
    pub address: SocketAddr,
    #[serde(default)]
    pub audio_only: bool,
    /// Unix seconds of the last connection; the list is newest first.
    #[serde(default)]
    pub last_used: u64,
}

#[derive(Serialize, Deserialize)]
struct SavedFile {
    version: u32,
    sessions: Vec<SavedSession>,
}

/// The saved sessions, newest first. Every change is written through to
/// disk; a failed write keeps the change in memory and reports why.
pub struct SavedSessions {
    path: Option<PathBuf>,
    sessions: Vec<SavedSession>,
}

impl SavedSessions {
    /// Loads the user's saved sessions. A missing file is an empty list; an
    /// unreadable one is moved aside so the next save cannot overwrite it.
    pub fn load() -> (Self, Option<String>) {
        let path = config_dir().map(|dir| dir.join(FILE_NAME));
        match Self::load_from(path.clone()) {
            Ok(saved) => (saved, None),
            Err(error) => {
                warn!("Unable to read saved sessions: {error}");
                if let Some(path) = &path {
                    let _ = fs::rename(path, path.with_extension("invalid.json"));
                }
                let empty = Self {
                    path,
                    sessions: Vec::new(),
                };
                (empty, Some(error))
            }
        }
    }

    fn load_from(path: Option<PathBuf>) -> Result<Self, String> {
        let mut sessions = match &path {
            Some(path) => match fs::read(path) {
                Ok(bytes) => {
                    serde_json::from_slice::<SavedFile>(&bytes)
                        .map_err(|error| format!("{}: {error}", path.display()))?
                        .sessions
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(format!("{}: {error}", path.display())),
            },
            None => Vec::new(),
        };
        sort_newest_first(&mut sessions);
        Ok(Self { path, sessions })
    }

    pub fn sessions(&self) -> &[SavedSession] {
        &self.sessions
    }

    pub fn find(&self, address: SocketAddr) -> Option<&SavedSession> {
        self.sessions
            .iter()
            .find(|session| session.address == address)
    }

    /// Saves `name` for the host at `address` and marks it as just used. A
    /// session already saved at `address` is renamed. Otherwise, when the
    /// form was loaded from the session saved at `previous` and kept its
    /// name, that session moves to the new address instead of duplicating.
    pub fn remember(
        &mut self,
        name: &str,
        address: SocketAddr,
        audio_only: bool,
        previous: Option<SocketAddr>,
    ) -> Result<(), String> {
        let session = SavedSession {
            name: clean_name(name),
            address,
            audio_only,
            last_used: unix_now(),
        };
        let existing = self
            .sessions
            .iter()
            .position(|saved| saved.address == address)
            .or_else(|| {
                let previous = previous?;
                self.sessions
                    .iter()
                    .position(|saved| saved.address == previous && saved.name == session.name)
            });
        match existing {
            Some(index) => self.sessions[index] = session,
            None => self.sessions.push(session),
        }
        sort_newest_first(&mut self.sessions);
        self.persist()
    }

    /// Whether `address` is saved under a session other than `original`.
    pub fn taken_by_another(&self, original: SocketAddr, address: SocketAddr) -> bool {
        address != original && self.find(address).is_some()
    }

    /// Replaces the session saved at `original` with `edited`, keeping when
    /// it was last used. Fails if `edited` moves onto another saved address.
    pub fn update(&mut self, original: SocketAddr, edited: SavedSession) -> Result<(), String> {
        if self.taken_by_another(original, edited.address) {
            return Err(format!(
                "Another saved session already uses {}.",
                edited.address
            ));
        }
        let Some(saved) = self
            .sessions
            .iter_mut()
            .find(|saved| saved.address == original)
        else {
            return Err("This session is no longer saved.".to_owned());
        };
        *saved = SavedSession {
            name: clean_name(&edited.name),
            last_used: saved.last_used,
            ..edited
        };
        self.persist()
    }

    pub fn remove(&mut self, address: SocketAddr) -> Result<(), String> {
        self.sessions.retain(|saved| saved.address != address);
        self.persist()
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err("No configuration directory is available.".to_owned());
        };
        let file = SavedFile {
            version: FORMAT_VERSION,
            sessions: self.sessions.clone(),
        };
        write_atomically(path, &file).map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// Writes beside the destination first so a crash mid-write never leaves a
/// truncated file in place of the user's data.
pub(crate) fn write_atomically(path: &Path, file: &impl Serialize) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
    let staging = path.with_extension("json.tmp");
    fs::write(&staging, json)?;
    fs::rename(&staging, path)
}

fn clean_name(name: &str) -> String {
    name.trim().chars().take(NAME_LIMIT).collect()
}

fn sort_newest_first(sessions: &mut [SavedSession]) {
    sessions.sort_by(|left, right| {
        right
            .last_used
            .cmp(&left.last_used)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(target_os = "macos")]
pub(crate) fn config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join("Library/Application Support/Echobind"))
}

#[cfg(target_os = "windows")]
pub(crate) fn config_dir() -> Option<PathBuf> {
    let app_data = std::env::var_os("APPDATA").filter(|dir| !dir.is_empty())?;
    Some(PathBuf::from(app_data).join("Echobind"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(base.join("echobind"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = format!(
                "echobind-saved-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            Self(std::env::temp_dir().join(unique))
        }

        fn file(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let dir = TempDir::new("missing");
        let saved = SavedSessions::load_from(Some(dir.file())).unwrap();
        assert!(saved.sessions().is_empty());
    }

    #[test]
    fn remembered_sessions_survive_a_reload() {
        let dir = TempDir::new("reload");
        let mut saved = SavedSessions::load_from(Some(dir.file())).unwrap();
        saved
            .remember("  Gaming PC  ", address("192.168.1.50:3013"), false, None)
            .unwrap();
        saved
            .remember("Studio", address("[fe80::1]:4000"), true, None)
            .unwrap();

        let reloaded = SavedSessions::load_from(Some(dir.file())).unwrap();
        assert_eq!(reloaded.sessions().len(), 2);
        let gaming = reloaded.find(address("192.168.1.50:3013")).unwrap();
        assert_eq!(gaming.name, "Gaming PC");
        assert!(!gaming.audio_only);
        assert!(reloaded.find(address("[fe80::1]:4000")).unwrap().audio_only);
    }

    #[test]
    fn remembering_a_saved_address_renames_it_instead_of_duplicating() {
        let dir = TempDir::new("rename");
        let mut saved = SavedSessions::load_from(Some(dir.file())).unwrap();
        saved
            .remember("Desk", address("10.0.0.2:3013"), false, None)
            .unwrap();
        saved
            .remember("Office desk", address("10.0.0.2:3013"), true, None)
            .unwrap();

        assert_eq!(saved.sessions().len(), 1);
        assert_eq!(saved.sessions()[0].name, "Office desk");
        assert!(saved.sessions()[0].audio_only);
    }

    #[test]
    fn a_kept_name_moves_the_session_it_was_loaded_from() {
        let dir = TempDir::new("move");
        let mut saved = SavedSessions::load_from(Some(dir.file())).unwrap();
        let old = address("192.168.1.50:3013");
        let new = address("192.168.1.77:3013");
        saved.remember("Gaming PC", old, false, None).unwrap();

        // Same name at a new address: the host's IP changed.
        saved.remember("Gaming PC", new, false, Some(old)).unwrap();
        assert_eq!(saved.sessions().len(), 1);
        assert_eq!(saved.sessions()[0].address, new);

        // A new name at another address is a different host.
        saved
            .remember("Laptop", address("192.168.1.90:3013"), false, Some(new))
            .unwrap();
        assert_eq!(saved.sessions().len(), 2);
        assert!(saved.find(new).is_some());
    }

    #[test]
    fn newest_sessions_come_first() {
        let mut sessions = vec![
            SavedSession {
                name: "old".to_owned(),
                address: address("10.0.0.1:1"),
                audio_only: false,
                last_used: 10,
            },
            SavedSession {
                name: "new".to_owned(),
                address: address("10.0.0.2:1"),
                audio_only: false,
                last_used: 20,
            },
        ];
        sort_newest_first(&mut sessions);
        assert_eq!(sessions[0].name, "new");
    }

    #[test]
    fn edits_keep_their_place_and_refuse_a_taken_address() {
        let dir = TempDir::new("edit");
        let mut saved = SavedSessions::load_from(Some(dir.file())).unwrap();
        saved
            .remember("A", address("10.0.0.1:3013"), false, None)
            .unwrap();
        saved
            .remember("B", address("10.0.0.2:3013"), false, None)
            .unwrap();
        let last_used = saved.find(address("10.0.0.1:3013")).unwrap().last_used;

        let taken = SavedSession {
            name: "A".to_owned(),
            address: address("10.0.0.2:3013"),
            audio_only: false,
            last_used: 0,
        };
        assert!(saved.update(address("10.0.0.1:3013"), taken).is_err());

        let moved = SavedSession {
            name: "Renamed".to_owned(),
            address: address("10.0.0.9:4000"),
            audio_only: true,
            last_used: 0,
        };
        saved.update(address("10.0.0.1:3013"), moved).unwrap();
        let edited = saved.find(address("10.0.0.9:4000")).unwrap();
        assert_eq!(edited.name, "Renamed");
        assert_eq!(edited.last_used, last_used);
        assert!(saved.find(address("10.0.0.1:3013")).is_none());
    }

    #[test]
    fn removed_sessions_stay_removed() {
        let dir = TempDir::new("remove");
        let mut saved = SavedSessions::load_from(Some(dir.file())).unwrap();
        saved
            .remember("A", address("10.0.0.1:3013"), false, None)
            .unwrap();
        saved.remove(address("10.0.0.1:3013")).unwrap();

        let reloaded = SavedSessions::load_from(Some(dir.file())).unwrap();
        assert!(reloaded.sessions().is_empty());
    }

    #[test]
    fn an_unparsable_file_is_reported() {
        let dir = TempDir::new("invalid");
        fs::create_dir_all(&dir.0).unwrap();
        fs::write(dir.file(), b"{ not json").unwrap();
        assert!(SavedSessions::load_from(Some(dir.file())).is_err());
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(clean_name("  Den  "), "Den");
        assert_eq!(
            clean_name(&"x".repeat(NAME_LIMIT + 10)).chars().count(),
            NAME_LIMIT
        );
    }
}
