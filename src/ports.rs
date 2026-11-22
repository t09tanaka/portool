//! TCP "is this port unused" checks, used both by block allocation (spec
//! §6.3) and by GC (spec §8.1) to detect ports in use by processes outside
//! the ledger.
//!
//! A port counts as free only when nothing listens on it in *any* form:
//! `127.0.0.1`, `0.0.0.0` (a published container port), `[::]` (dual-stack
//! or v6-only) or `[::1]` (`localhost` on an IPv6-first resolver). The probe
//! binds both wildcard addresses **without** `SO_REUSEADDR`: with that option
//! (which `std::net::TcpListener::bind` always sets) a bind of `127.0.0.1`
//! succeeds on macOS while another socket listens on `0.0.0.0`, and no bind
//! of an IPv4 address can see a listener on `[::1]`. Nothing is connected
//! to and nothing ever listens, so the probe takes no timeouts.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{FromRawFd, OwnedFd};

/// Whether `port` is currently unused, determined by binding it on the IPv4
/// and the IPv6 wildcard address. The sockets are closed immediately, so
/// this is inherently TOCTOU (spec §6.3 acknowledges this). Errs towards
/// "in use": on Linux a port whose server just stopped can read as in use
/// until its last connections leave `TIME_WAIT`.
pub fn port_free(port: u16) -> bool {
    wildcard_bindable(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)
        && wildcard_bindable(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)
}

/// Whether every port in the inclusive range `block` is free.
pub fn block_free(block: (u16, u16)) -> bool {
    (block.0..=block.1).all(port_free)
}

/// Whether something listens on `block` as seen from `127.0.0.1` alone, the
/// way every release up to 0.13 probed. For the places that look at a
/// worktree's *own* block (`exec --check-ports`, `doctor`'s report): there
/// the listener is normally the worktree's own stack, and what those
/// commands warn about or move on must not change. Never use it to decide
/// that a block can be handed out or released -- that is [`block_free`].
pub fn loopback_block_free(block: (u16, u16)) -> bool {
    (block.0..=block.1).all(|port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok())
}

/// Binds `ip:port` on a fresh socket with no `SO_REUSEADDR` (and, for IPv6,
/// `IPV6_V6ONLY` so the two probes cover disjoint address spaces).
///
/// IPv4: any failure means "not free", as before. IPv6: a host without IPv6
/// (no socket of that family, or no address to bind) is not a reason to call
/// every port busy, so only those specific failures count as free.
fn wildcard_bindable(ip: IpAddr, port: u16) -> bool {
    let v6 = ip.is_ipv6();
    let family = if v6 { libc::AF_INET6 } else { libc::AF_INET };
    // SAFETY: plain socket calls; the descriptor is owned by `_fd` from the
    // moment it exists and closed when this function returns.
    unsafe {
        let raw = libc::socket(family, libc::SOCK_STREAM, 0);
        if raw < 0 {
            return v6 && no_ipv6(std::io::Error::last_os_error().raw_os_error());
        }
        let _fd = OwnedFd::from_raw_fd(raw);
        libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
        if v6 {
            let only: libc::c_int = 1;
            libc::setsockopt(
                raw,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                (&raw const only).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let (addr, len) = sockaddr(ip, port);
        if libc::bind(raw, (&raw const addr).cast(), len) == 0 {
            return true;
        }
        v6 && no_ipv6(std::io::Error::last_os_error().raw_os_error())
    }
}

/// The errors that mean "this host has no usable IPv6", as opposed to "the
/// port is taken" (`EADDRINUSE`) or anything unexpected (treated as taken).
fn no_ipv6(errno: Option<i32>) -> bool {
    matches!(
        errno,
        Some(libc::EAFNOSUPPORT | libc::EPROTONOSUPPORT | libc::EADDRNOTAVAIL)
    )
}

/// A `sockaddr` for `ip:port`, with its length.
fn sockaddr(ip: IpAddr, port: u16) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: `sockaddr_storage` is plain data, valid when zeroed, and large
    // and aligned enough for both address families.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match ip {
        IpAddr::V4(v4) => {
            let sin = (&raw mut storage).cast::<libc::sockaddr_in>();
            // SAFETY: see above.
            unsafe {
                (*sin).sin_family = libc::AF_INET as libc::sa_family_t;
                (*sin).sin_port = port.to_be();
                (*sin).sin_addr.s_addr = u32::from(v4).to_be();
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        IpAddr::V6(v6) => {
            let sin6 = (&raw mut storage).cast::<libc::sockaddr_in6>();
            // SAFETY: see above.
            unsafe {
                (*sin6).sin6_family = libc::AF_INET6 as libc::sa_family_t;
                (*sin6).sin6_port = port.to_be();
                (*sin6).sin6_addr.s6_addr = v6.octets();
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    /// These tests use `bind(0)` to obtain a genuinely free ephemeral port
    /// and then immediately release it to check `port_free`/`block_free`
    /// against it. `cargo test`'s default parallelism means the OS could
    /// otherwise hand that just-released port to some other concurrently
    /// running test (in this crate or elsewhere on the machine) before
    /// this one re-checks it; serializing the tests here plus a
    /// short bounded retry on the "now free" assertions absorbs that
    /// transient contention. Production `port_free`/`block_free` remain
    /// inherently TOCTOU against processes outside this test binary, as
    /// documented above -- this is purely test robustness, not a change
    /// to that behavior.
    static PORT_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_guard() -> std::sync::MutexGuard<'static, ()> {
        PORT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Retries `check` for up to ~1s, on the assumption that any conflict
    /// on a just-released ephemeral port is transient.
    fn eventually(mut check: impl FnMut() -> bool) -> bool {
        for _ in 0..50 {
            if check() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A listening socket opened the way real servers open one
    /// (`SO_REUSEADDR` set), on an OS-chosen port. `None` when the address
    /// family or address is unavailable on this host (no IPv6), so the
    /// caller skips that form.
    fn listen(ip: IpAddr, v6only: bool) -> Option<(OwnedFd, u16)> {
        let family = if ip.is_ipv6() {
            libc::AF_INET6
        } else {
            libc::AF_INET
        };
        // SAFETY: plain socket calls on a descriptor owned by `fd`.
        unsafe {
            let raw = libc::socket(family, libc::SOCK_STREAM, 0);
            if raw < 0 {
                return None;
            }
            let fd = OwnedFd::from_raw_fd(raw);
            let len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let one: libc::c_int = 1;
            libc::setsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&raw const one).cast(),
                len,
            );
            if ip.is_ipv6() {
                let only: libc::c_int = v6only.into();
                libc::setsockopt(
                    raw,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&raw const only).cast(),
                    len,
                );
            }
            let (addr, addr_len) = sockaddr(ip, 0);
            if libc::bind(raw, (&raw const addr).cast(), addr_len) != 0 || libc::listen(raw, 8) != 0
            {
                return None;
            }
            let mut out: libc::sockaddr_storage = std::mem::zeroed();
            let mut out_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            if libc::getsockname(raw, (&raw mut out).cast(), &mut out_len) != 0 {
                return None;
            }
            let port = if ip.is_ipv6() {
                u16::from_be((*(&raw const out).cast::<libc::sockaddr_in6>()).sin6_port)
            } else {
                u16::from_be((*(&raw const out).cast::<libc::sockaddr_in>()).sin_port)
            };
            Some((fd, port))
        }
    }

    /// Every way a dev server or a container runtime listens must count as
    /// "in use": `127.0.0.1`, `0.0.0.0` (published container ports), `[::]`
    /// dual-stack (Node's default) and v6-only, and `[::1]` (`localhost` on
    /// an IPv6-first resolver).
    #[test]
    fn every_listener_form_counts_as_in_use() {
        let _guard = lock_guard();
        let forms: [(&str, IpAddr, bool); 5] = [
            ("127.0.0.1", Ipv4Addr::LOCALHOST.into(), false),
            ("0.0.0.0", Ipv4Addr::UNSPECIFIED.into(), false),
            ("[::] dual-stack", Ipv6Addr::UNSPECIFIED.into(), false),
            ("[::] v6only", Ipv6Addr::UNSPECIFIED.into(), true),
            ("[::1]", Ipv6Addr::LOCALHOST.into(), true),
        ];
        let mut checked = Vec::new();
        for (name, ip, v6only) in forms {
            let Some((listener, port)) = listen(ip, v6only) else {
                assert!(ip.is_ipv6(), "{name}: an IPv4 listener is always possible");
                continue; // no IPv6 on this host
            };
            assert!(!port_free(port), "{name}: port {port} read as free");
            assert!(!block_free((port, port)), "{name}: block read as free");
            drop(listener);
            assert!(
                eventually(|| port_free(port)),
                "{name}: port {port} never became free"
            );
            checked.push(name);
        }
        assert!(checked.len() >= 2, "IPv4 forms not checked: {checked:?}");
    }

    #[test]
    fn only_a_missing_ipv6_stack_is_ignored() {
        assert!(no_ipv6(Some(libc::EAFNOSUPPORT)));
        assert!(no_ipv6(Some(libc::EADDRNOTAVAIL)));
        assert!(!no_ipv6(Some(libc::EADDRINUSE)));
        assert!(!no_ipv6(Some(libc::EACCES)));
        assert!(!no_ipv6(None));
    }

    #[test]
    fn port_free_is_false_while_bound_and_true_after_release() {
        let _guard = lock_guard();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(!port_free(port));

        drop(listener);

        assert!(
            eventually(|| port_free(port)),
            "port {port} never became free"
        );
    }

    #[test]
    fn block_free_true_for_a_released_port() {
        let _guard = lock_guard();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        assert!(
            eventually(|| block_free((port, port))),
            "block ({port}, {port}) never became free"
        );
    }

    #[test]
    fn block_free_false_when_any_port_in_block_is_occupied() {
        let _guard = lock_guard();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let block = if port < u16::MAX - 1 {
            (port, port + 1)
        } else {
            (port - 1, port)
        };

        assert!(!block_free(block));
    }
}
