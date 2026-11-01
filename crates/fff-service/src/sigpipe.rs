use std::io;

use interprocess::local_socket::tokio::SendHalf;

// Writes to a closed connection fail with EPIPE instead of raising SIGPIPE, which ends a host
// program that does not ignore it. On Linux the standard library already writes to Unix sockets
// with MSG_NOSIGNAL; macOS has no such flag, only this socket option.
#[cfg(target_vendor = "apple")]
pub(crate) fn suppress(send: &SendHalf) -> io::Result<()> {
    match send {
        SendHalf::UdSocket(half) => nosigpipe(half),
    }
}

#[cfg(not(target_vendor = "apple"))]
pub(crate) fn suppress(_send: &SendHalf) -> io::Result<()> {
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn nosigpipe(socket: impl std::os::fd::AsFd) -> io::Result<()> {
    rustix::net::sockopt::set_socket_nosigpipe(socket, true).map_err(io::Error::from)
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    // The test harness ignores SIGPIPE; a host program may not.
    struct DefaultSigpipe;

    impl DefaultSigpipe {
        fn new() -> Self {
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
            Self
        }
    }

    impl Drop for DefaultSigpipe {
        fn drop(&mut self) {
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
        }
    }

    #[test]
    fn a_write_to_a_closed_connection_fails_without_a_signal() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        #[cfg(target_vendor = "apple")]
        super::nosigpipe(&ours).unwrap();
        drop(theirs);
        let _default = DefaultSigpipe::new();
        let error = ours.write_all(b"frame").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
