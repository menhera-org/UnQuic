# UnQuic Encapsulation

- Naming: `UnQuic`, because the transport borrows ideas from QUIC without being QUIC.
- 'Encapsulation' instead of 'VPN': people blindly expect a VPN to encrypt, and 'VPN' is a commercially contaminated marketing word.
- Development language: pure Rust.
- Dependencies: none except `libc`. Everything else (config parsing, UUID generation, congestion control, DNS via `getaddrinfo`) is written in-tree. Write C-like Rust, optimized for performance.
- Requirements: none except a Linux kernel. Binaries are built for `*-unknown-linux-musl` targets.
- Design: see [initial-design.md](initial-design.md).
