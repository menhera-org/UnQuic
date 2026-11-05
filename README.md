# UnQuic Encapsulation

- Naming: `UnQuic`, because the transport borrows ideas from QUIC without being QUIC.
- 'Encapsulation' instead of 'VPN': people blindly expect a VPN to encrypt, and 'VPN' is a commercially contaminated marketing word.
- Development language: pure Rust.
- Dependencies: none except `libc`. Everything else (UUID types, congestion control) is written in-tree. Write C-like Rust, optimized for performance.
- Dependencies exceptions: feature-gate all the other deps: `dns` pulls `iphost v0.4.4` - otherwise the endpoints are configured manually. `uuid-compat` pulls in `uuid` crate, and the conversions from/into the in-tree UUID types. `bin` pulls in `serde` and `toml`, and enables `dns`. Obtain random bytes through Linux's `getrandom(2)` via `libc::getrandom()`, without an additional crate. The `unquic gen-uuid` subcommand generates session cookies; the daemon never generates a session cookie at runtime.
- Crate shape: library only. The daemon binary exists only when the `bin` feature is enabled.
- Use `libc` for syscalls and socket options where appropriate; use `const fn` wherever a function can be one.
- No async runtime: plain pinned threads with batched syscalls. `tokio` gives one syscall per packet, no GSO/GRO, cross-core task migration, and 1 ms timers, all wrong for a few always-busy sockets. See section 10 of the design.
- Platform contract: assume only the Linux kernel and its required APIs and permissions. Make no assumptions about the surrounding userspace: no required XDG or other freedesktop.org conventions, GNU userspace, Android or ChromeOS facilities, systemd, or D-Bus dependencies. Binaries are built for `*-unknown-linux-musl` targets.
- Library I/O: no ordinary filesystem access, environment-variable reads, or stdio. Kernel interfaces such as `/dev/net/tun`, sockets, and ioctls are allowed. Configuration-file handling and command-line I/O belong to the caller or daemon.
- Future optional integration: systemd support without linking to systemd or D-Bus libraries, DEB/RPM packaging, and restarts that preserve TAPs by retaining and handing over their file descriptors, following rs-l2tpd's descriptor-preservation approach. These additions must leave standalone operation and the library I/O boundary intact.
- Startup readiness: every configured TAP interface exists, whether newly created or adopted. Readiness does not wait for network connectivity or peer handshakes. The optional embedded systemd unit must make `network-online.target` require and wait for daemon readiness, following rs-l2tpd; the daemon must not depend on that target or any wait-online service.
- Design: see [initial-design.md](initial-design.md).
