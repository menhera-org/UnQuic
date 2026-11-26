# Wire version 1 vectors

Each hex file contains one complete UDP payload with one record. Whitespace
separates bytes and has no protocol meaning. These are handwritten fixtures,
checked byte for byte against both record and datagram encoders by
`tests/wire.rs`.

All fixtures use cookie `000102030405060708090a0b0c0d0e0f`.
Established packets use receive tag `101112131415161718191a1b1c1d1e1f`,
lane `0x1234`, and packet number `0x0102030405060708`.
Handshake packets have zero tag, lane, and packet number.

| File | Record and fixture values |
| - | - |
| padding.hex | Three zero body bytes |
| data.hex | Frame 0x1234000000000009, total 14, offset 10, bytes de ad be ef |
| ack.hex | Lane 0x5678, delay 1000 µs, inclusive ranges 256..258 and 7..9 |
| hello.hex | Initiator tag 202122232425262728292a2b2c2d2e2f |
| challenge.hex | Same initiator; responder tag 303132333435363738393a3b3c3d3e3f |
| probe.hex | Empty body |
| probe_reply.hex | Lane 0x5678, packet 0x0102030405060708, UDP payload 1232 |
| frame_abandon.hex | Lane 0x1234, sender origin, resource-limit reason, frame 0x1234000000000009 |
| confirm.hex | The initiator/responder pair above |
| ready.hex | The initiator/responder pair above |
| lane_retire.hex | Lane 0x5678 |
| data_offload.hex | DATA flags 1: the same frame as an offload frame of 2964 bytes, offset 0, carrying its 10-byte descriptor (partial checksum, TCP over IPv6, 74 header bytes, 1440-byte segments, checksum at 54 + 16) |
| data_partial.hex | DATA flags 2: data.hex's piece of a frame with a partial checksum |
| data_striped.hex | DATA flags 4: the same frame as a striped frame of 22 bytes, offset 0, carrying its 8-byte tag (stripe 5, sequence 1) |
| hello_features.hex | hello.hex advertising optional features 7: `OFFLOAD_FRAMES`, `PARTIAL_CHECKSUMS` and `STRIPED_FLOWS` |

HELLO and CHALLENGE advertise receive port 40000, frame ceiling 9022, and zero
mandatory/optional feature bits unless the table says otherwise. The last four
rows fix the optional extensions' encodings (design section 6): a peer that
advertises none of them never receives a flagged DATA record. These fixtures
describe v1's current encoding; state-machine vectors remain later roadmap
work.
