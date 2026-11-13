// SPDX-License-Identifier: EUPL-1.2
//! Versioned preview policy, kept separate from the released settings schema.
use super::{Profile, atomic_private_write};
use nuschl_link_preview::{policy::Settings, settings_file::{self, Loaded, Origin, Source}};
use std::{fs::File, io::{self, Read}, path::{Path, PathBuf}};

#[derive(Clone, Debug)]
pub struct LinkPreviewSettings {
    /// The user's own route, beneath any managed restriction.
    pub user: Loaded,
    pub effective: Loaded,
}

fn load(path: &Path, origin: Origin) -> Loaded {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return settings_file::parse(origin, Source::Missing);
        }
        Err(_) => return settings_file::parse(origin, Source::Unreadable),
        Ok(_) => {}
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return settings_file::parse(origin, Source::Unreadable),
    };
    let mut bytes = Vec::new();
    match file.take(settings_file::MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes) {
        Ok(_) => settings_file::parse(origin, Source::Bytes(&bytes)),
        Err(_) => settings_file::parse(origin, Source::Unreadable),
    }
}

pub(super) fn system_managed() -> Option<PathBuf> {
    if !cfg!(all(unix, not(target_os = "macos"))) {
        return None;
    }
    crate::layout::xdg_config_dirs(std::env::var_os)
        .into_iter()
        .flat_map(|directory| {
            ["nuschl", "nuschel"]
                .map(|name| directory.join(name).join(settings_file::MANAGED_FILE_NAME))
        })
        // A permissions/I/O failure must not bypass a managed restriction.
        .find(|path| match std::fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        })
}

impl Profile {
    /// Load bounded sidecars without rewriting, renaming or repairing them.
    /// Explicit profiles never consult the host's managed configuration.
    pub fn load_link_previews(&self) -> LinkPreviewSettings {
        let user = load(&self.config_dir.join(settings_file::FILE_NAME), Origin::User);
        let mut effective = user.clone();
        let inherited = self.system_link_previews.clone().or_else(|| {
            self.system_managed.as_ref().and_then(|path| path.parent())
                .map(|directory| directory.join(settings_file::MANAGED_FILE_NAME))
        });
        for path in inherited.into_iter().chain([
            self.config_dir.join(settings_file::MANAGED_FILE_NAME),
        ]) {
            let managed = load(&path, Origin::Managed);
            effective = settings_file::effective(&effective, &managed);
        }
        LinkPreviewSettings { user, effective }
    }

    /// Save an explicit local preference action, never a startup/default save.
    /// This deliberately replaces the user's sidecar; managed files are read
    /// again and can only narrow the result. No addresses appear in errors.
    pub fn save_link_previews(&self, settings: &Settings) -> io::Result<LinkPreviewSettings> {
        let bytes = settings_file::serialize(settings).map_err(io::Error::other)?;
        atomic_private_write(&self.config_dir.join(settings_file::FILE_NAME), &bytes)?;
        Ok(self.load_link_previews())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_link_preview::{policy::Mode, settings_file::Problem};

    #[test]
    fn explicit_profile_keeps_unknown_files_and_only_explicit_save_replaces_them() {
        let root = tempfile::tempdir_in(".").unwrap();
        let profile = Profile::open(Some(root.path().to_owned())).unwrap();
        let path = profile.config_dir.join(settings_file::FILE_NAME);
        let future = br#"{"version":999,"secret_future_field":true}"#;
        std::fs::write(&path, future).unwrap();
        let loaded = profile.load_link_previews();
        assert_eq!(loaded.effective.settings.mode, Mode::Off);
        assert!(matches!(loaded.user.problem, Some(Problem::Version(Origin::User,999))));
        assert_eq!(std::fs::read(&path).unwrap(), future);
        let saved = profile.save_link_previews(&Settings { mode: Mode::Direct, ..Settings::default() }).unwrap();
        assert_eq!(saved.effective.settings.mode, Mode::Direct);
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn every_managed_layer_narrows_and_broken_or_oversized_layers_fail_closed() {
        let root = tempfile::tempdir_in(".").unwrap();
        let mut profile = Profile::open(Some(root.path().to_owned())).unwrap();
        let system = root.path().join("system-link-previews.json");
        profile.system_link_previews = Some(system.clone());
        let own = profile.config_dir.join(settings_file::MANAGED_FILE_NAME);
        std::fs::write(&system, br#"{"version":1,"mode":"off"}"#).unwrap();
        std::fs::write(&own, br#"{"version":1,"mode":"direct"}"#).unwrap();
        assert_eq!(profile.save_link_previews(&Settings {mode:Mode::Direct,..Settings::default()}).unwrap().effective.settings.mode,Mode::Off);
        for bytes in [b"invalid".to_vec(),vec![b' ';settings_file::MAX_FILE_BYTES+1]] {
            std::fs::write(&system,&bytes).unwrap();
            let loaded=profile.load_link_previews();
            assert_eq!(loaded.effective.settings.mode,Mode::Off);
            assert!(loaded.effective.problem.is_some());
            assert_eq!(std::fs::read(&system).unwrap(),bytes);
        }
    }
}
