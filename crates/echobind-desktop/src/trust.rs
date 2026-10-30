//! Who this device is when it watches, and which viewers it lets back in
//! without asking when it shares. Both are kept as JSON beside the saved
//! sessions.

use crate::saved::{config_dir, unix_now, write_atomically};
use echobind_core::protocol::ViewerId;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs, io,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tracing::warn;

const IDENTITY_FILE_NAME: &str = "viewer-id.json";
const TRUSTED_FILE_NAME: &str = "trusted-viewers.json";
const FORMAT_VERSION: u32 = 1;

/// The viewer IDs a sharing host accepts without asking. The app owns the
/// list; the host's network thread only reads it.
pub type TrustedIds = Arc<RwLock<HashSet<ViewerId>>>;

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    version: u32,
    viewer_id: ViewerId,
}

/// This device's viewer ID, created on first use. When it can't be saved,
/// hosts can only trust this device until Echobind quits.
pub fn local_viewer_id() -> ViewerId {
    viewer_id_at(config_dir().map(|dir| dir.join(IDENTITY_FILE_NAME)))
}

fn viewer_id_at(path: Option<PathBuf>) -> ViewerId {
    let Some(path) = path else {
        warn!("No configuration directory; hosts can trust this device only until Echobind quits");
        return random_viewer_id();
    };
    match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<IdentityFile>(&bytes) {
            Ok(file) => return file.viewer_id,
            // Nothing else reads this file, so a damaged one is replaced.
            Err(error) => warn!("Replacing unreadable {}: {error}", path.display()),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!("Unable to read {}: {error}", path.display());
            return random_viewer_id();
        }
    }
    let viewer_id = random_viewer_id();
    let file = IdentityFile {
        version: FORMAT_VERSION,
        viewer_id,
    };
    if let Err(error) = write_atomically(&path, &file) {
        warn!(
            "Unable to save {}: {error}; hosts can trust this device only until Echobind quits",
            path.display()
        );
    }
    viewer_id
}

fn random_viewer_id() -> ViewerId {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).expect("the operating system's random source failed");
    ViewerId(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedViewer {
    pub id: ViewerId,
    /// Where the viewer last connected from, so the host can recognize it.
    pub address: IpAddr,
    /// Unix seconds of its last connection; the list is newest first.
    pub last_seen: u64,
}

#[derive(Serialize, Deserialize)]
struct TrustedFile {
    version: u32,
    viewers: Vec<TrustedViewer>,
}

/// The viewers this host accepts without asking, newest first. Every change
/// is written through to disk and to the IDs a running host checks; a failed
/// write keeps the change in memory and reports why.
pub struct TrustedViewers {
    path: Option<PathBuf>,
    viewers: Vec<TrustedViewer>,
    ids: TrustedIds,
}

impl TrustedViewers {
    /// Loads the trusted viewers. A missing file trusts no one; an unreadable
    /// one is moved aside so the next change cannot overwrite it.
    pub fn load() -> (Self, Option<String>) {
        let path = config_dir().map(|dir| dir.join(TRUSTED_FILE_NAME));
        match Self::load_from(path.clone()) {
            Ok(trusted) => (trusted, None),
            Err(error) => {
                warn!("Unable to read trusted viewers: {error}");
                if let Some(path) = &path {
                    let _ = fs::rename(path, path.with_extension("invalid.json"));
                }
                (Self::with(path, Vec::new()), Some(error))
            }
        }
    }

    fn load_from(path: Option<PathBuf>) -> Result<Self, String> {
        let viewers = match &path {
            Some(path) => match fs::read(path) {
                Ok(bytes) => {
                    serde_json::from_slice::<TrustedFile>(&bytes)
                        .map_err(|error| format!("{}: {error}", path.display()))?
                        .viewers
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(format!("{}: {error}", path.display())),
            },
            None => Vec::new(),
        };
        Ok(Self::with(path, viewers))
    }

    fn with(path: Option<PathBuf>, mut viewers: Vec<TrustedViewer>) -> Self {
        sort_newest_first(&mut viewers);
        // A hand-edited file may list a viewer twice; its newest entry wins.
        let mut seen = HashSet::new();
        viewers.retain(|viewer| seen.insert(viewer.id));
        let ids = Arc::new(RwLock::new(
            viewers.iter().map(|viewer| viewer.id).collect(),
        ));
        Self { path, viewers, ids }
    }

    pub fn viewers(&self) -> &[TrustedViewer] {
        &self.viewers
    }

    pub fn contains(&self, id: ViewerId) -> bool {
        self.viewers.iter().any(|viewer| viewer.id == id)
    }

    /// The IDs a host checks, kept in step with every change.
    pub fn ids(&self) -> TrustedIds {
        self.ids.clone()
    }

    /// Trusts `id`, or marks an already trusted viewer as just seen at
    /// `address`.
    pub fn trust(&mut self, id: ViewerId, address: IpAddr) -> Result<(), String> {
        let viewer = TrustedViewer {
            id,
            address,
            last_seen: unix_now(),
        };
        match self.viewers.iter_mut().find(|trusted| trusted.id == id) {
            Some(trusted) => *trusted = viewer,
            None => self.viewers.push(viewer),
        }
        self.changed()
    }

    pub fn forget(&mut self, id: ViewerId) -> Result<(), String> {
        self.viewers.retain(|viewer| viewer.id != id);
        self.changed()
    }

    fn changed(&mut self) -> Result<(), String> {
        sort_newest_first(&mut self.viewers);
        *self.ids.write().unwrap() = self.viewers.iter().map(|viewer| viewer.id).collect();
        let Some(path) = &self.path else {
            return Err("No configuration directory is available.".to_owned());
        };
        let file = TrustedFile {
            version: FORMAT_VERSION,
            viewers: self.viewers.clone(),
        };
        write_atomically(path, &file).map_err(|error| format!("{}: {error}", path.display()))
    }
}

fn sort_newest_first(viewers: &mut [TrustedViewer]) {
    viewers.sort_by(|left, right| {
        right
            .last_seen
            .cmp(&left.last_seen)
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = format!(
                "echobind-trust-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            Self(std::env::temp_dir().join(unique))
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_viewer_id_is_created_once_and_kept() {
        let dir = TempDir::new("identity");
        let path = dir.file(IDENTITY_FILE_NAME);
        let first = viewer_id_at(Some(path.clone()));
        assert!(path.exists());
        assert_eq!(viewer_id_at(Some(path)), first);
    }

    #[test]
    fn a_damaged_viewer_id_is_replaced() {
        let dir = TempDir::new("identity-damaged");
        fs::create_dir_all(&dir.0).unwrap();
        let path = dir.file(IDENTITY_FILE_NAME);
        fs::write(&path, b"{ not json").unwrap();
        let replaced = viewer_id_at(Some(path.clone()));
        assert_eq!(viewer_id_at(Some(path)), replaced);
    }

    #[test]
    fn viewer_ids_differ_between_devices() {
        assert_ne!(random_viewer_id(), random_viewer_id());
    }

    #[test]
    fn trusted_viewers_survive_a_reload_and_reach_the_host() {
        let dir = TempDir::new("reload");
        let path = dir.file(TRUSTED_FILE_NAME);
        let mut trusted = TrustedViewers::load_from(Some(path.clone())).unwrap();
        let ids = trusted.ids();
        let laptop = ViewerId([1; 16]);
        trusted.trust(laptop, ip("192.168.1.20")).unwrap();
        assert!(ids.read().unwrap().contains(&laptop));

        let reloaded = TrustedViewers::load_from(Some(path)).unwrap();
        assert!(reloaded.contains(laptop));
        assert_eq!(reloaded.viewers()[0].address, ip("192.168.1.20"));
        assert!(reloaded.ids().read().unwrap().contains(&laptop));
    }

    #[test]
    fn trusting_again_updates_the_address_instead_of_duplicating() {
        let dir = TempDir::new("update");
        let mut trusted = TrustedViewers::load_from(Some(dir.file(TRUSTED_FILE_NAME))).unwrap();
        let laptop = ViewerId([1; 16]);
        trusted.trust(laptop, ip("192.168.1.20")).unwrap();
        trusted.trust(laptop, ip("192.168.1.44")).unwrap();
        assert_eq!(trusted.viewers().len(), 1);
        assert_eq!(trusted.viewers()[0].address, ip("192.168.1.44"));
    }

    #[test]
    fn forgotten_viewers_need_approval_again() {
        let dir = TempDir::new("forget");
        let path = dir.file(TRUSTED_FILE_NAME);
        let mut trusted = TrustedViewers::load_from(Some(path.clone())).unwrap();
        let ids = trusted.ids();
        let laptop = ViewerId([1; 16]);
        let phone = ViewerId([2; 16]);
        trusted.trust(laptop, ip("192.168.1.20")).unwrap();
        trusted.trust(phone, ip("192.168.1.21")).unwrap();
        trusted.forget(laptop).unwrap();

        assert!(!ids.read().unwrap().contains(&laptop));
        assert!(ids.read().unwrap().contains(&phone));
        let reloaded = TrustedViewers::load_from(Some(path)).unwrap();
        assert!(!reloaded.contains(laptop));
        assert!(reloaded.contains(phone));
    }

    #[test]
    fn newest_viewers_come_first_and_duplicates_collapse() {
        let viewer = |byte, last_seen| TrustedViewer {
            id: ViewerId([byte; 16]),
            address: ip("10.0.0.1"),
            last_seen,
        };
        let trusted = TrustedViewers::with(
            None,
            vec![viewer(1, 10), viewer(2, 20), viewer(1, 30), viewer(3, 5)],
        );
        let order: Vec<_> = trusted
            .viewers()
            .iter()
            .map(|viewer| (viewer.id.0[0], viewer.last_seen))
            .collect();
        assert_eq!(order, [(1, 30), (2, 20), (3, 5)]);
    }

    #[test]
    fn a_change_without_a_config_dir_still_applies_in_memory() {
        let mut trusted = TrustedViewers::with(None, Vec::new());
        let laptop = ViewerId([1; 16]);
        assert!(trusted.trust(laptop, ip("10.0.0.1")).is_err());
        assert!(trusted.contains(laptop));
        assert!(trusted.ids().read().unwrap().contains(&laptop));
    }

    #[test]
    fn an_unparsable_file_is_reported() {
        let dir = TempDir::new("invalid");
        fs::create_dir_all(&dir.0).unwrap();
        let path = dir.file(TRUSTED_FILE_NAME);
        fs::write(&path, b"{ not json").unwrap();
        assert!(TrustedViewers::load_from(Some(path)).is_err());
    }
}
