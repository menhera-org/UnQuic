# Handshake state-machine vectors, wire version 1

Each file is a transcript for two endpoints, `a` and `b`. A line gives one
input to one endpoint and, after `=>`, everything that input must produce.
`tests/handshake.rs` replays every file and compares it byte for byte.

```
<milliseconds> <endpoint> <input> [fresh=<tag>] => <effects>; <state>
```

Times are milliseconds on one monotonic clock that both endpoints share.
Tags are hexadecimal. `fresh=` is the unused random tag the endpoint has at
hand for this input; without it the endpoint has none.

| Input | Meaning |
| - | - |
| `timer` | The endpoint's timer runs |
| `cancel` | The endpoint's configured path changed and it drops its unconfirmed attempt |
| `feedback` | A new acknowledgement or probe reply arrived for the confirmed connection |
| `recv hello I` | A HELLO with initiator tag `I` arrived |
| `recv challenge I R` | A CHALLENGE with initiator tag `I` and responder tag `R` arrived |
| `recv confirm I R` | A CONFIRM arrived |
| `recv ready I R` | A READY arrived |

Effects appear in the order their owner applies them, and only those that
happen:

| Effect | Meaning |
| - | - |
| `retire` | Retire the confirmed connection's transport state |
| `commit initiator`, `commit responder` | Install the connection with this local role |
| `carrier up`, `carrier down` | Change the TAP's carrier |
| `send <record>` | Send this one handshake record |
| `needs-tag` | Nothing happened for lack of a fresh tag |
| `error (<reason>)` | The input was rejected and changed nothing |

The state that ends each line is `configured`, `establishing`, `active`,
`degraded` or `inactive` (design section 5.3).

Endpoint `a` advertises receive port 40000 and `b` 40001, both a frame
ceiling of 9022 and no feature bits, and each requires 1514-byte frames. A
received HELLO or CHALLENGE carries its sender's parameters;
`port=<number>` after the tags changes the port it advertises.
[../wire-v1](../wire-v1/README.md) fixes how the records are encoded.

| File | What it fixes |
| - | - |
| establish.txt | The four-message exchange, liveness, and a replacement begun on a degraded connection |
| simultaneous.txt | Both endpoints starting at once, and a HELLO that arrives after the exchange it lost |
| equal-tags.txt | Both endpoints drawing the same initiator tag |
| loss.txt | Repeated and lost records, the one-second limits, and rejected parameters |
| expiry.txt | An unanswered attempt, its ten-second life, tag reuse and cancellation |
| replacement.txt | A replacement begun by the old responder, thirty seconds of silence, and a new connection from inactive |
