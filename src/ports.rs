//! TCP "is this port unused" checks, used both by block allocation (spec
//! §6.3) and by GC (spec §8.1) to detect ports in use by processes outside
//! the ledger.
//!
//! A port counts as free only when nothing listens on it in *any* form:
//! `127.0.0.1`, `0.0.0.0` (a published container port), `[::]` (dual-stack
//! or v6-only), `[::1]` (`localhost` on an IPv6-first resolver) or another
//! address of this host. And a port whose server has just stopped counts as
//! free at once, although the kernel keeps its last connections for a while
//! (`TIME_WAIT`: about 30 s on macOS, 60 s on Linux).
//!
//! The probe, per address family ([`family_free`]):
//!
//! 1. Bind the wildcard address **without** `SO_REUSEADDR`. Success: nothing
//!    at all is bound to the port, free. (With the option, which
//!    `std::net::TcpListener::bind` always sets, a bind succeeds on macOS
//!    while another socket listens on a different address of the port.)
//! 2. "Address in use" is either a listener or left-over connections. Bind
//!    the wildcard address again **with** `SO_REUSEADDR`: left-over
//!    connections do not stop that, a wildcard listener does -- and on
//!    Linux a listener on any address does.
//! 3. On macOS and the BSDs step 2 succeeds beside a listener on a specific
//!    address, so each address of this host (loopback first) is bound with
//!    `SO_REUSEADDR` as well; "address in use" on any of them is a listener.
//!
//! Nothing is connected to and nothing ever listens, so the probe takes no
//! timeouts and no server sees it.
//!
//! One case still reads as in use until `TIME_WAIT` ends, on Linux only: a
//! server that did not set `SO_REUSEADDR` itself (its left-over connections
//! then block step 2). Most servers set it, since they could not restart
//! quickly otherwise.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Whether `port` is currently unused: nothing listens on it on any IPv4 or
/// IPv6 address of this host (see the module documentation for the probe).
/// The sockets are closed immediately, so this is inherently TOCTOU (spec
/// §6.3 acknowledges this). Errs towards "in use".
pub fn port_free(port: u16) -> bool {
    family_free(false, port) && family_free(true, port)
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

/// What one bind attempt showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bind {
    Bound,
    /// `EADDRINUSE`.
    InUse,
    /// This host has no usable address of the family.
    NoFamily,
    /// Anything else (`EACCES` for a privileged port ...).
    Failed,
}

/// The probe for one address family; see the module documentation.
///
/// IPv4: any unexpected failure means "not free". IPv6: a host without IPv6
/// (no socket of that family, or no address to bind) is not a reason to
/// call every port busy, so exactly those failures count as free.
fn family_free(v6: bool, port: u16) -> bool {
    let wildcard: IpAddr = match v6 {
        true => Ipv6Addr::UNSPECIFIED.into(),
        false => Ipv4Addr::UNSPECIFIED.into(),
    };
    let (addr, len) = sockaddr(wildcard, port);
    match bind(v6, &addr, len, false) {
        Bind::Bound => return true,
        Bind::NoFamily => return v6,
        Bind::Failed => return false,
        Bind::InUse => {}
    }
    if bind(v6, &addr, len, true) != Bind::Bound {
        return false;
    }
    let Some(locals) = local_addresses(v6, port) else {
        return false; // cannot rule out a listener on a specific address
    };
    locals
        .iter()
        .all(|(addr, len)| bind(v6, addr, *len, true) != Bind::InUse)
}

/// A fresh stream socket of the family, close-on-exec from the moment it
/// exists where the platform can do that in one call.
fn probe_socket(v6: bool) -> Result<OwnedFd, Bind> {
    let family = if v6 { libc::AF_INET6 } else { libc::AF_INET };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: plain socket calls; the descriptor is owned by `fd` from the
    // moment it exists.
    unsafe {
        let raw = libc::socket(family, kind, 0);
        if raw < 0 {
            return Err(failure(v6));
        }
        let fd = OwnedFd::from_raw_fd(raw);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok(fd)
    }
}

/// Binds `addr` on a fresh socket, with or without `SO_REUSEADDR` (and, for
/// IPv6, always `IPV6_V6ONLY`, so the two families cover disjoint address
/// spaces). The socket is closed when this returns.
fn bind(v6: bool, addr: &libc::sockaddr_storage, len: libc::socklen_t, reuse: bool) -> Bind {
    let fd = match probe_socket(v6) {
        Ok(fd) => fd,
        Err(bind) => return bind,
    };
    let raw = fd.as_raw_fd();
    let one: libc::c_int = 1;
    let option = |level, name| {
        // SAFETY: `raw` is open and `one` outlives the call.
        unsafe {
            libc::setsockopt(
                raw,
                level,
                name,
                (&raw const one).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    if v6 {
        option(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY);
    }
    if reuse && option(libc::SOL_SOCKET, libc::SO_REUSEADDR) != 0 {
        return Bind::Failed;
    }
    // SAFETY: `addr` is a valid socket address of `len` bytes.
    if unsafe { libc::bind(raw, std::ptr::from_ref(addr).cast(), len) } == 0 {
        return Bind::Bound;
    }
    failure(v6)
}

/// Classifies the error of the socket call that just failed.
fn failure(v6: bool) -> Bind {
    let errno = std::io::Error::last_os_error().raw_os_error();
    if errno == Some(libc::EADDRINUSE) {
        Bind::InUse
    } else if v6 && no_ipv6(errno) {
        Bind::NoFamily
    } else {
        Bind::Failed
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

/// Every address of the family this host has (`getifaddrs`), loopback
/// first, each with `port` filled in. `None` when they cannot be listed.
fn local_addresses(v6: bool, port: u16) -> Option<Vec<(libc::sockaddr_storage, libc::socklen_t)>> {
    let loopback: IpAddr = match v6 {
        true => Ipv6Addr::LOCALHOST.into(),
        false => Ipv4Addr::LOCALHOST.into(),
    };
    let mut out = vec![sockaddr(loopback, port)];
    let (family, len) = match v6 {
        true => (libc::AF_INET6, std::mem::size_of::<libc::sockaddr_in6>()),
        false => (libc::AF_INET, std::mem::size_of::<libc::sockaddr_in>()),
    };
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `list` with a linked list that stays valid
    // until `freeifaddrs`; every `ifa_addr` of the wanted family points to
    // at least `len` bytes, which fit into a zeroed `sockaddr_storage`.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let item = &*cursor;
            if !item.ifa_addr.is_null() && i32::from((*item.ifa_addr).sa_family) == family {
                let mut storage: libc::sockaddr_storage = std::mem::zeroed();
                std::ptr::copy_nonoverlapping(
                    item.ifa_addr.cast::<u8>(),
                    (&raw mut storage).cast::<u8>(),
                    len,
                );
                if v6 {
                    (*(&raw mut storage).cast::<libc::sockaddr_in6>()).sin6_port = port.to_be();
                } else {
                    (*(&raw mut storage).cast::<libc::sockaddr_in>()).sin_port = port.to_be();
                }
                out.push((storage, len as libc::socklen_t));
            }
            cursor = item.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    Some(out)
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
    /// (`SO_REUSEADDR` set) or without the option, on an OS-chosen port. `None` when the address
    /// family or address is unavailable on this host (no IPv6), so the
    /// caller skips that form.
    fn listen_with(ip: IpAddr, v6only: bool, reuse: bool) -> Option<(OwnedFd, u16)> {
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
            let one: libc::c_int = reuse.into();
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
        // With and without `SO_REUSEADDR` on the listener: the second bind
        // of the probe must not mistake either for left-over connections.
        for reuse in [true, false] {
            let mut checked = Vec::new();
            for (name, ip, v6only) in forms {
                let Some((listener, port)) = listen_with(ip, v6only, reuse) else {
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
            // No silent skip: where the IPv6 loopback can be bound at all,
            // all five forms must have been exercised.
            if std::net::TcpListener::bind("[::1]:0").is_ok() {
                assert_eq!(checked.len(), 5, "IPv6 forms skipped: {checked:?}");
            }
        }
    }

    /// An address of this host that is not loopback, if it has one.
    fn lan_address() -> Option<IpAddr> {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        // Nothing is sent: connecting a UDP socket only picks a route.
        probe.connect("192.0.2.1:9").ok()?;
        let ip = probe.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    #[test]
    fn a_listener_on_another_local_address_counts_as_in_use() {
        let _guard = lock_guard();
        let Some(ip) = lan_address() else {
            return; // this host has only loopback
        };
        for reuse in [true, false] {
            let (listener, port) = listen_with(ip, false, reuse).expect("a local address");
            assert!(!port_free(port), "{ip}: port {port} read as free");
            drop(listener);
            assert!(eventually(|| port_free(port)));
        }
    }

    /// A server (opened the way `std` and most servers do, `SO_REUSEADDR`
    /// set) accepts one connection, closes it first and stops: its side of
    /// the connection is now in `TIME_WAIT`. Returns the port.
    fn stopped_server(ip: IpAddr) -> Option<u16> {
        use std::io::Read;
        let server = TcpListener::bind((ip, 0)).ok()?;
        let port = server.local_addr().unwrap().port();
        let mut client = std::net::TcpStream::connect((ip, port)).unwrap();
        let (accepted, _) = server.accept().unwrap();
        drop(accepted); // the server closes first: the active close
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).unwrap(); // the client saw it
        drop(client);
        drop(server);
        Some(port)
    }

    /// `TIME_WAIT` lasts 30 to 60 seconds. The checks below allow the one
    /// second this module allows everywhere for a port that was just
    /// closed (a child process another test is spawning at that moment
    /// holds a copy of the socket until its `exec`), which still tells the
    /// two apart.
    #[test]
    fn a_port_whose_server_just_stopped_is_free_at_once() {
        let _guard = lock_guard();
        let port = stopped_server(Ipv4Addr::LOCALHOST.into()).unwrap();
        assert!(
            eventually(|| port_free(port)),
            "port {port} of a stopped server reads as in use"
        );
        // Measured on macOS: the plain bind is still refused (for about
        // 30 s), so it is the second step of the probe that answered.
        #[cfg(target_os = "macos")]
        {
            let (addr, len) = sockaddr(Ipv4Addr::UNSPECIFIED.into(), port);
            assert_eq!(bind(false, &addr, len, false), Bind::InUse);
        }
        assert!(eventually(|| block_free((port, port))));

        if let Some(port) = stopped_server(Ipv6Addr::LOCALHOST.into()) {
            assert!(
                eventually(|| port_free(port)),
                "[::1]:{port} of a stopped server reads as in use"
            );
        }
        let port = stopped_server(Ipv4Addr::UNSPECIFIED.into()).unwrap();
        assert!(
            eventually(|| port_free(port)),
            "0.0.0.0:{port} of a stopped server reads as in use"
        );
    }

    #[test]
    fn a_stopped_server_does_not_hide_a_new_listener() {
        let _guard = lock_guard();
        let port = stopped_server(Ipv4Addr::LOCALHOST.into()).unwrap();
        assert!(eventually(|| port_free(port)));
        // The server is restarted on the same port, as servers are.
        let restarted = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        assert!(!port_free(port));
        drop(restarted);
        assert!(eventually(|| port_free(port)));
    }

    #[test]
    fn the_probe_socket_is_close_on_exec_and_the_local_addresses_are_listed() {
        let fd = probe_socket(false).unwrap();
        // SAFETY: `fd` is open.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        let locals = local_addresses(false, 4321).unwrap();
        assert!(!locals.is_empty());
        let first = (&raw const locals[0].0).cast::<libc::sockaddr_in>();
        // SAFETY: an IPv4 socket address was written there.
        let (port, ip) = unsafe { ((*first).sin_port, (*first).sin_addr.s_addr) };
        assert_eq!(u16::from_be(port), 4321);
        assert_eq!(Ipv4Addr::from(u32::from_be(ip)), Ipv4Addr::LOCALHOST);
        // Every listed address carries the port.
        for (addr, _) in &locals {
            let sin = std::ptr::from_ref(addr).cast::<libc::sockaddr_in>();
            // SAFETY: as above.
            assert_eq!(u16::from_be(unsafe { (*sin).sin_port }), 4321);
        }
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
