use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{FromRawFd, RawFd};

fn main() {
    // The host can finish first-launch loader work before the timed adapter
    // handshake. This mode uses no descriptors and performs no socket calls.
    if std::env::args().nth(1).as_deref() == Some("--startup-probe") {
        return;
    }
    let descriptor: RawFd = std::env::args().nth(1).unwrap().parse().unwrap();
    // SAFETY: the fixture passes ownership of its inherited listener.
    let original = unsafe { TcpListener::from_raw_fd(descriptor) };
    let listener = original.try_clone().unwrap();
    drop(original);
    listener.set_nonblocking(false).unwrap();
    let mut outgoing = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    outgoing.write_all(b"rust inheritance").unwrap();
    let (mut incoming, _) = listener.accept().unwrap();
    let mut bytes = [0; 16];
    incoming.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"rust inheritance");
}
