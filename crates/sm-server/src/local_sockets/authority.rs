//! Host registration and kernel-derived authorization for the socket service.

use super::identity::{denied, snapshot, PeerToken, ProcessIdentity, ProcessSnapshot};
use std::{
    collections::HashMap,
    io,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
};

const MAX_AGENTS: usize = 128;
const MAX_ROOTS_PER_AGENT: usize = 32;
const MAX_ANCESTORS: usize = 128;

#[derive(Default)]
struct Registrations {
    agents: HashMap<String, u16>,
    roots: HashMap<ProcessIdentity, RootRegistration>,
    next_registration: u64,
}

struct RootRegistration {
    agent: String,
    serial: u64,
}

/// Shared trusted-host authority. Agents cannot add roots through the wire
/// protocol. Every socket endpoint must share this authority so a nested root
/// registered to another agent cannot inherit its ancestor's permissions.
#[derive(Clone, Default)]
pub struct Authority(Arc<Mutex<Registrations>>);

/// Authorization of one connection, tied to an exact root registration.
/// Removing and re-adding the same live process does not revive old connections.
pub struct RootCapability {
    authority: Authority,
    agent: String,
    root: ProcessIdentity,
    serial: u64,
}

impl Authority {
    pub fn register_agent(&self, agent: &str, control_port: u16) -> io::Result<()> {
        if agent.is_empty()
            || agent.len() > 64
            || !agent
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            || control_port == 0
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut registrations = self.0.lock().map_err(|_| denied())?;
        if registrations.agents.contains_key(agent)
            || registrations
                .agents
                .values()
                .any(|port| *port == control_port)
        {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        if registrations.agents.len() >= MAX_AGENTS {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        registrations.agents.insert(agent.to_owned(), control_port);
        Ok(())
    }

    pub fn unregister_agent(&self, agent: &str) -> io::Result<()> {
        let mut registrations = self.0.lock().map_err(|_| denied())?;
        registrations.agents.remove(agent);
        registrations
            .roots
            .retain(|_, registration| registration.agent != agent);
        Ok(())
    }

    pub fn add_root(&self, agent: &str, root: ProcessIdentity) -> io::Result<()> {
        // SAFETY: geteuid has no pointer arguments and cannot fail.
        if root.uid != unsafe { libc::geteuid() } || !root.is_live() {
            return Err(denied());
        }
        let mut registrations = self.0.lock().map_err(|_| denied())?;
        if !registrations.agents.contains_key(agent) {
            return Err(denied());
        }
        // Even idempotent additions are rejected: the host must explicitly
        // remove a registration before changing its lifetime or ownership.
        if registrations.roots.contains_key(&root) {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        if registrations
            .roots
            .values()
            .filter(|registration| registration.agent == agent)
            .count()
            >= MAX_ROOTS_PER_AGENT
        {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        registrations.next_registration = registrations
            .next_registration
            .checked_add(1)
            .ok_or_else(denied)?;
        let serial = registrations.next_registration;
        registrations.roots.insert(
            root,
            RootRegistration {
                agent: agent.to_owned(),
                serial,
            },
        );
        Ok(())
    }

    pub fn remove_root(&self, agent: &str, root: ProcessIdentity) -> io::Result<()> {
        let mut registrations = self.0.lock().map_err(|_| denied())?;
        if registrations
            .roots
            .get(&root)
            .is_some_and(|registration| registration.agent == agent)
        {
            registrations.roots.remove(&root);
            Ok(())
        } else {
            Err(denied())
        }
    }

    pub fn authorize(&self, agent: &str, stream: &UnixStream) -> io::Result<RootCapability> {
        self.authorize_snapshot(agent, PeerToken::read(stream)?.process()?)
    }

    fn authorize_snapshot(
        &self,
        agent: &str,
        mut process: ProcessSnapshot,
    ) -> io::Result<RootCapability> {
        let registrations = self.0.lock().map_err(|_| denied())?;
        if !registrations.agents.contains_key(agent) {
            return Err(denied());
        }
        // Hold registration ownership stable throughout the ancestry walk.
        // The kernel's parent unique ID prevents a recycled parent PID from
        // manufacturing an ancestry relationship between separate snapshots.
        for _ in 0..MAX_ANCESTORS {
            if let Some(registration) = registrations.roots.get(&process.identity) {
                if registration.agent != agent || !process.identity.is_live() {
                    return Err(denied());
                }
                return Ok(RootCapability {
                    authority: self.clone(),
                    agent: agent.to_owned(),
                    root: process.identity,
                    serial: registration.serial,
                });
            }
            if process.parent_pid == 0 || process.parent_pid == process.identity.pid {
                break;
            }
            let parent = snapshot(process.parent_pid)?;
            if parent.identity.unique_id != process.parent_unique_id {
                return Err(denied());
            }
            process = parent;
        }
        Err(denied())
    }
}

impl RootCapability {
    /// Workers check this before transferring descriptors and during idle
    /// waits. A connection may survive its connecting descendant's exit, but
    /// never its registered root's exit or host revocation.
    pub fn is_live(&self) -> bool {
        self.authority.0.lock().is_ok_and(|registrations| {
            registrations
                .roots
                .get(&self.root)
                .is_some_and(|registration| {
                    registration.agent == self.agent && registration.serial == self.serial
                })
                && self.root.is_live()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn spawn_child() -> ChildGuard {
        ChildGuard(Command::new("/bin/sleep").arg("60").spawn().unwrap())
    }
    fn authority() -> Authority {
        let authority = Authority::default();
        authority.register_agent("one", 20001).unwrap();
        authority.register_agent("two", 20002).unwrap();
        authority
    }

    #[test]
    fn nearest_registered_root_prevents_cross_agent_ancestry() {
        let authority = authority();
        let parent = ProcessIdentity::capture(std::process::id()).unwrap();
        authority.add_root("one", parent).unwrap();
        let child = spawn_child();
        let root = ProcessIdentity::capture(child.0.id()).unwrap();
        assert!(authority
            .authorize_snapshot("one", snapshot(root.pid()).unwrap())
            .unwrap()
            .is_live());
        authority.add_root("two", root).unwrap();
        assert!(authority
            .authorize_snapshot("one", snapshot(root.pid()).unwrap())
            .is_err());
        assert!(authority
            .authorize_snapshot("two", snapshot(root.pid()).unwrap())
            .unwrap()
            .is_live());
        assert!(authority.add_root("one", root).is_err());
        assert!(authority.remove_root("one", root).is_err());
    }

    #[test]
    fn revocation_and_restore_do_not_revive_old_connections() {
        let authority = authority();
        let root = ProcessIdentity::capture(std::process::id()).unwrap();
        authority.add_root("one", root).unwrap();
        let capability = authority
            .authorize_snapshot("one", snapshot(root.pid()).unwrap())
            .unwrap();
        authority.remove_root("one", root).unwrap();
        assert!(!capability.is_live());
        authority.add_root("one", root).unwrap();
        assert!(!capability.is_live());
        let restored = authority
            .authorize_snapshot("one", snapshot(root.pid()).unwrap())
            .unwrap();
        authority.unregister_agent("one").unwrap();
        authority.register_agent("one", 20001).unwrap();
        authority.add_root("one", root).unwrap();
        assert!(!restored.is_live());
    }

    #[test]
    fn root_exit_and_forged_parent_generation_fail_closed() {
        let authority = authority();
        let mut child = spawn_child();
        let root = ProcessIdentity::capture(child.0.id()).unwrap();
        authority.add_root("one", root).unwrap();
        let capability = authority
            .authorize_snapshot("one", snapshot(root.pid()).unwrap())
            .unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!capability.is_live());
        assert!(authority.add_root("two", root).is_err());
        let parent = ProcessIdentity::capture(std::process::id()).unwrap();
        authority.add_root("two", parent).unwrap();
        let child = spawn_child();
        let mut process = snapshot(child.0.id()).unwrap();
        process.parent_unique_id = process.parent_unique_id.wrapping_add(1);
        assert!(authority.authorize_snapshot("two", process).is_err());
    }

    #[test]
    fn host_registration_rejects_unknown_agents_and_duplicate_ports() {
        let authority = authority();
        let root = ProcessIdentity::capture(std::process::id()).unwrap();
        assert!(authority.add_root("unknown", root).is_err());
        assert!(authority.register_agent("three", 20001).is_err());
        assert!(authority.register_agent("invalid/path", 20003).is_err());
        assert!(authority.register_agent("zero", 0).is_err());
        assert!(authority
            .authorize_snapshot("one", snapshot(root.pid()).unwrap())
            .is_err());
    }
}
