use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::process::{Child, Command};

// Descriptors another test opens can reach a child before their close-on-exec flag is set, so a
// child marks every descriptor but its standard streams and those it is given close-on-exec.
pub trait CommandExt {
    fn spawn_isolated(&mut self) -> io::Result<Child>;
    // As `spawn_isolated`, and the child also inherits `keep`.
    #[cfg(unix)]
    fn spawn_keeping(&mut self, keep: &[RawFd]) -> io::Result<Child>;
}

impl CommandExt for Command {
    #[cfg(unix)]
    fn spawn_isolated(&mut self) -> io::Result<Child> {
        self.spawn_keeping(&[])
    }

    // Windows creates a child's pipes inheritable only while it starts that child.
    #[cfg(windows)]
    fn spawn_isolated(&mut self) -> io::Result<Child> {
        self.spawn()
    }

    #[cfg(unix)]
    fn spawn_keeping(&mut self, keep: &[RawFd]) -> io::Result<Child> {
        use std::os::unix::process::CommandExt;
        let bound = descriptor_bound();
        let keep = keep.to_vec();
        // SAFETY: the closure only makes system calls, which are async-signal-safe, and
        // allocates nothing.
        unsafe {
            self.pre_exec(move || {
                mark_close_on_exec(bound);
                for &fd in &keep {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    }
                }
                Ok(())
            });
        }
        self.spawn()
    }
}

#[cfg(target_os = "linux")]
unsafe fn mark_close_on_exec(bound: RawFd) {
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    // SAFETY: close_range only changes descriptor flags.
    let done = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        )
    };
    if done != 0 {
        unsafe { sweep(bound) };
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
unsafe fn mark_close_on_exec(bound: RawFd) {
    unsafe { sweep(bound) };
}

#[cfg(unix)]
unsafe fn sweep(bound: RawFd) {
    for fd in 3..bound {
        // SAFETY: fcntl on a descriptor that does not exist fails and changes nothing.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

// One past the highest descriptor the test process can hold.
#[cfg(unix)]
fn descriptor_bound() -> RawFd {
    static BOUND: std::sync::OnceLock<RawFd> = std::sync::OnceLock::new();
    *BOUND.get_or_init(|| {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit and setrlimit only read and write the struct they are given.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            // macOS has no close_range, so the sweep's cost follows the limit.
            #[cfg(target_os = "macos")]
            if limit.rlim_cur > 4096 {
                limit.rlim_cur = 4096;
                assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
            }
        }
        if limit.rlim_cur == libc::RLIM_INFINITY {
            return system_maximum();
        }
        RawFd::try_from(limit.rlim_cur).unwrap_or(RawFd::MAX)
    })
}

// What an unlimited soft limit allows, as the service's own sweep bounds it.
#[cfg(target_os = "linux")]
fn system_maximum() -> RawFd {
    std::fs::read_to_string("/proc/sys/fs/nr_open")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1 << 20)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn system_maximum() -> RawFd {
    let mut maximum: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: sysctlbyname writes at most `size` bytes into `maximum`.
    let found = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut maximum as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if found && maximum > 0 {
        maximum
    } else {
        1 << 20
    }
}
