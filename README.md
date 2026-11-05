# UnQuic Encapsulation

- Naming: `UnQuic`, from the fact that we got some inspiration from the QUIC protocol.
- 'Encapsulation' instead of 'VPN': because people expect VPNs of encryptions blindly, and the term 'VPN' is commercially contaminated as a marketing word.
- Development language: Pure-Rust.
- Dependencies: None except `libc`. Write C-like Rust code, optimized for performance.
- Requirements: None except a kernel Linux. bins are compiled to `*-unknown-linux-musl` targets.
