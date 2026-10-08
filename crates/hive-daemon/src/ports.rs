//! Per-worktree port slots: the main checkout is slot 0 and keeps the
//! configured ports; worktree N runs on `base + N * stride`.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// Lowest slot ≥ 1 not in `used`.
pub fn next_free_slot(used: &[u16]) -> u16 {
    (1..).find(|s| !used.contains(s)).unwrap()
}

pub fn port_for(base: u16, slot: u16, stride: u16) -> u16 {
    base.saturating_add(slot.saturating_mul(stride))
}

/// Is something already listening on `port`? Checks both a bind on all
/// interfaces and a connect to the loopbacks (servers bound to `::1` only
/// don't block an IPv4 bind).
pub fn in_use(port: u16) -> bool {
    if TcpListener::bind(("0.0.0.0", port)).is_err() {
        return true;
    }
    for addr in [
        SocketAddr::from(([127, 0, 0, 1], port)),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
    ] {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(80)).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots() {
        assert_eq!(next_free_slot(&[]), 1);
        assert_eq!(next_free_slot(&[0, 1, 3]), 2);
        assert_eq!(port_for(3000, 0, 10), 3000);
        assert_eq!(port_for(3000, 2, 10), 3020);
        assert_eq!(port_for(65530, 9, 10), 65535);
    }

    #[test]
    fn probe() {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(in_use(port));
        drop(l);
    }
}
