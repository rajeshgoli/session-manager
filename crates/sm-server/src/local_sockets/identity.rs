//! macOS kernel identities; no caller-supplied process/environment attribution.

use std::{
    io,
    mem::{size_of, MaybeUninit},
    os::{fd::AsRawFd, unix::net::UnixStream},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) unique_id: u64,
    pub(crate) pid_version: i32,
    pub(crate) started_seconds: u64,
    pub(crate) started_microseconds: u64,
    pub(crate) uid: u32,
}

impl ProcessIdentity {
    pub fn capture(pid: u32) -> io::Result<Self> {
        Ok(snapshot(pid)?.identity)
    }

    pub fn pid(self) -> u32 {
        self.pid
    }

    pub(crate) fn is_live(self) -> bool {
        Self::capture(self.pid).is_ok_and(|current| current == self)
    }
}

pub(crate) struct ProcessSnapshot {
    pub identity: ProcessIdentity,
    pub parent_pid: u32,
    pub parent_unique_id: u64,
}

// Apple XNU bsd/sys/proc_info_private.h: PROC_PIDT_BSDINFOWITHUNIQID
// atomically returns both records. The unique identifier ABI is 56 bytes.
#[repr(C)]
struct UniqueInfo {
    uuid: [u8; 16],
    unique_id: u64,
    parent_unique_id: u64,
    pid_version: i32,
    original_parent_pid_version: i32,
    reserved: [u64; 2],
}

#[repr(C)]
struct CombinedInfo {
    bsd: libc::proc_bsdinfo,
    unique: UniqueInfo,
}

const _: () = assert!(size_of::<UniqueInfo>() == 56);

pub(crate) fn snapshot(pid: u32) -> io::Result<ProcessSnapshot> {
    let pid_signed = i32::try_from(pid).map_err(|_| denied())?;
    if pid_signed <= 0 {
        return Err(denied());
    }
    let mut info = MaybeUninit::<CombinedInfo>::zeroed();
    // SAFETY: the kernel writes into the correctly sized C-layout record.
    // We inspect it only when the entire documented record was returned.
    let copied = unsafe {
        libc::proc_pidinfo(
            pid_signed,
            18, // PROC_PIDT_BSDINFOWITHUNIQID
            0,
            info.as_mut_ptr().cast(),
            size_of::<CombinedInfo>() as i32,
        )
    };
    if copied != size_of::<CombinedInfo>() as i32 {
        return Err(denied());
    }
    // SAFETY: exact-size successful proc_pidinfo initialized the record.
    let info = unsafe { info.assume_init() };
    if info.bsd.pbi_pid != pid || info.unique.unique_id == 0 {
        return Err(denied());
    }
    Ok(ProcessSnapshot {
        identity: ProcessIdentity {
            pid,
            unique_id: info.unique.unique_id,
            pid_version: info.unique.pid_version,
            started_seconds: info.bsd.pbi_start_tvsec,
            started_microseconds: info.bsd.pbi_start_tvusec,
            uid: info.bsd.pbi_uid,
        },
        parent_pid: info.bsd.pbi_ppid,
        parent_unique_id: info.unique.parent_unique_id,
    })
}

/// Full kernel audit token from a Unix socket. It includes the process-ID
/// generation and can be compared inside the wall without reading host process
/// information. The host stores this in the immutable launch configuration.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerToken(pub [u32; 8]);

#[link(name = "bsm")]
extern "C" {
    fn audit_token_to_euid(token: PeerToken) -> libc::uid_t;
    fn audit_token_to_pid(token: PeerToken) -> libc::pid_t;
    fn audit_token_to_pidversion(token: PeerToken) -> libc::c_int;
}

impl PeerToken {
    pub fn read(stream: &UnixStream) -> io::Result<Self> {
        let mut token = Self([0; 8]);
        let mut length = size_of::<Self>() as libc::socklen_t;
        // SAFETY: token is a live C-layout audit_token_t-sized output buffer.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERTOKEN,
                (&mut token as *mut Self).cast(),
                &mut length,
            )
        };
        if result != 0 || length as usize != size_of::<Self>() {
            return Err(denied());
        }
        Ok(token)
    }

    pub fn verify(self, stream: &UnixStream) -> io::Result<()> {
        if Self::read(stream)? != self {
            return Err(denied());
        }
        Ok(())
    }

    pub(crate) fn process(self) -> io::Result<ProcessSnapshot> {
        // SAFETY: token came from LOCAL_PEERTOKEN and libbsm's public functions
        // decode the opaque audit_token_t by value.
        let (pid, uid, version) = unsafe {
            (
                audit_token_to_pid(self),
                audit_token_to_euid(self),
                audit_token_to_pidversion(self),
            )
        };
        let current = snapshot(u32::try_from(pid).map_err(|_| denied())?)?;
        if current.identity.pid_version != version || current.identity.uid != uid {
            return Err(denied());
        }
        Ok(current)
    }
}

pub(crate) fn denied() -> io::Error {
    io::Error::from_raw_os_error(libc::EACCES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::net::UnixListener,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn process_capture_has_generation_and_rejects_stale_identity() {
        let identity = ProcessIdentity::capture(process::id()).unwrap();
        assert!(identity.is_live());
        assert_ne!(identity.unique_id, 0);
        let mut stale = identity;
        stale.pid_version = stale.pid_version.wrapping_add(1);
        assert!(!stale.is_live());
        assert!(ProcessIdentity::capture(0).is_err());
    }

    #[test]
    fn socket_peer_token_matches_kernel_process_generation() {
        let path = std::env::temp_dir().join(format!(
            "sm-socket-token-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        let token = PeerToken::read(&client).unwrap();
        assert_eq!(
            token.process().unwrap().identity,
            ProcessIdentity::capture(process::id()).unwrap()
        );
        token.verify(&server).unwrap();
        let mut forged = token;
        forged.0[7] = forged.0[7].wrapping_add(1);
        assert!(forged.verify(&client).is_err());
        drop(listener);
        fs::remove_file(path).unwrap();
    }
}
