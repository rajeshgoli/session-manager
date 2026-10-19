//! Files pasted through the owner's browser terminal. Retirement removes them.
use super::*;

pub(crate) const MAX_TERMINAL_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub(crate) fn terminal_image_extension(mime: &str, bytes: &[u8]) -> Result<&'static str> {
    let extension = match mime {
        "image/png" if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => "png",
        "image/jpeg" if bytes.starts_with(b"\xff\xd8\xff") => "jpg",
        "image/gif" if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => "gif",
        "image/webp" if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") => "webp",
        _ => anyhow::bail!("Paste a PNG, JPEG, GIF, or WebP image with matching file contents"),
    };
    anyhow::ensure!(
        bytes.len() <= MAX_TERMINAL_IMAGE_BYTES,
        "Image exceeds the 10 MiB limit"
    );
    Ok(extension)
}

impl SessionStore {
    fn terminal_image_parent(&self, session: &SessionRecord, create: bool) -> Result<PathBuf> {
        anyhow::ensure!(
            !session.id.is_empty()
                && session
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid image session identifier"
        );
        if session.provider == "opencode" {
            let binding = session
                .opencode
                .as_ref()
                .context("Local agent has no runtime binding")?;
            binding.validate()?;
            // The wall grants access to this exact temp folder, not the server's /tmp.
            let parent = PathBuf::from(&binding.state_dir).join("tmp");
            anyhow::ensure!(
                parent.canonicalize()? == parent,
                "Local temp folder has path aliases"
            );
            Ok(parent)
        } else {
            let parent = self
                .state_file
                .parent()
                .context("Session state has no parent")?
                .join("terminal-images")
                .join(&session.id);
            if create {
                fs::create_dir_all(&parent)?;
            }
            Ok(parent.canonicalize()?)
        }
    }

    /// Shares the registry write lock with retirement: retirement cannot leave
    /// a just-uploaded image behind, and uploads cannot revive a retired folder.
    #[cfg(unix)]
    pub(crate) fn save_terminal_image(
        &self,
        id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<PathBuf> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let extension = terminal_image_extension(mime, bytes)?;
        let _guard = self.write_guard()?;
        let session = self.get_session(id)?.context("Session not found")?;
        anyhow::ensure!(!session.is_stopped(), "Session is not running");
        anyhow::ensure!(
            is_primary_node(&session.node),
            "Image paste requires an agent on this server"
        );
        let parent = self.terminal_image_parent(&session, true)?;
        let parent_fd = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(&parent)?;
        // Descriptor-relative creation prevents a sandboxed agent replacing the
        // upload directory with a symlink and making the server write outside it.
        let name = c"pasted-images";
        if unsafe { nix::libc::mkdirat(parent_fd.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        let directory = unsafe {
            nix::libc::openat(
                parent_fd.as_raw_fd(),
                name.as_ptr(),
                nix::libc::O_RDONLY
                    | nix::libc::O_DIRECTORY
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_CLOEXEC,
            )
        };
        if directory < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let directory = unsafe { fs::File::from_raw_fd(directory) };
        let filename = format!("paste-{}.{extension}", generate_session_id());
        let c_filename = std::ffi::CString::new(filename.clone())?;
        let fd = unsafe {
            nix::libc::openat(
                directory.as_raw_fd(),
                c_filename.as_ptr(),
                nix::libc::O_WRONLY
                    | nix::libc::O_CREAT
                    | nix::libc::O_EXCL
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut file = unsafe { fs::File::from_raw_fd(fd) };
        file.write_all(bytes)?;
        Ok(parent.join("pasted-images").join(filename))
    }

    #[cfg(not(unix))]
    pub(crate) fn save_terminal_image(
        &self,
        _id: &str,
        _mime: &str,
        _bytes: &[u8],
    ) -> Result<PathBuf> {
        anyhow::bail!("Terminal image paste requires Unix")
    }

    pub(super) fn cleanup_terminal_images(&self, id: &str) -> Result<()> {
        let Some(session) = self.get_session(id)? else {
            return Ok(());
        };
        let parent = match self.terminal_image_parent(&session, false) {
            Ok(parent) => parent,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error),
        };
        let directory = parent.join("pasted-images");
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(directory)?,
            Ok(_) => fs::remove_file(directory)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if session.provider != "opencode" {
            fs::remove_dir(parent)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        store: SessionStore,
    }
    impl Fixture {
        fn new(local: bool) -> Self {
            let root =
                std::env::temp_dir().join(format!("sm-terminal-images-{}", generate_session_id()));
            fs::create_dir_all(root.join("local/tmp")).unwrap();
            let root = root.canonicalize().unwrap();
            let state_file = root.join("sessions.json");
            let binding = local.then(|| json!({"port":18500,"state_dir":root.join("local"),"version":"1.17.9","model_base_url":"http://127.0.0.1:8000/v1"}));
            fs::write(&state_file, json!({"sessions":[{"id":"agent1","name":"agent1","working_dir":"/tmp","tmux_session":"agent1","provider":if local {"opencode"} else {"claude"},"node":"primary","status":"running","opencode":binding,"created_at":"2026-10-18T00:00:00","last_activity":"2026-10-18T00:00:00"}]}).to_string()).unwrap();
            Self {
                root,
                store: SessionStore::new(state_file),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn terminal_images_validate_formats_and_size() {
        for (mime, data, extension) in [
            ("image/png", b"\x89PNG\r\n\x1a\n".as_slice(), "png"),
            ("image/jpeg", b"\xff\xd8\xff".as_slice(), "jpg"),
            ("image/gif", b"GIF89a".as_slice(), "gif"),
            ("image/webp", b"RIFFxxxxWEBP".as_slice(), "webp"),
        ] {
            assert_eq!(terminal_image_extension(mime, data).unwrap(), extension);
        }
        assert!(terminal_image_extension("image/jpeg", b"GIF89a").is_err());
        assert!(terminal_image_extension("image/svg+xml", b"<svg/>").is_err());
        let mut image = vec![0; MAX_TERMINAL_IMAGE_BYTES];
        image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(terminal_image_extension("image/png", &image).is_ok());
        image.push(0);
        assert!(terminal_image_extension("image/png", &image).is_err());
    }

    #[test]
    fn terminal_images_use_agent_folder_and_retirement_cleans_up() {
        for local in [false, true] {
            let f = Fixture::new(local);
            let bytes = b"\x89PNG\r\n\x1a\nfixture";
            let path = f
                .store
                .save_terminal_image("agent1", "image/png", bytes)
                .unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert!(path.starts_with(if local {
                f.root.join("local/tmp")
            } else {
                f.root.join("terminal-images/agent1")
            }));
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            // The no-runtime retire path exercises shared retirement cleanup.
            assert!(matches!(
                f.store.retire_core_session("agent1", None).unwrap(),
                CoreRetireOutcome::Retired(_)
            ));
            assert!(!path.exists());
            assert!(f
                .store
                .save_terminal_image("agent1", "image/png", bytes)
                .is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminal_images_reject_symlinked_local_upload_directory() {
        let f = Fixture::new(true);
        let outside = f.root.join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, f.root.join("local/tmp/pasted-images")).unwrap();
        assert!(f
            .store
            .save_terminal_image("agent1", "image/png", b"\x89PNG\r\n\x1a\n")
            .is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        f.store.cleanup_terminal_images("agent1").unwrap();
        assert!(outside.is_dir());
    }
}
