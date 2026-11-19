# Secure protocol runs on iroh (QUIC)

For the new protocol we chose iroh over a custom UDP protocol: it gives authenticated encryption, hole punching and relay fallback (which also covers the TCP/443 case) without us writing them. Audio goes in QUIC datagrams, control on one stream, ALPN `sonobus/2`, on its own UDP port (10999) next to the legacy 10998.

## Considered Options

- **Custom protocol on UDP**: least latency overhead and full control, rejected because we would own the cryptography and NAT traversal.

## Consequences

- QUIC's default congestion control (Cubic) measured p50 2.65 s latency under loss, so we use a fixed-window controller and do our own adaptation (`sonobus-net/src/adapt.rs`): drop redundancy and quality when congested, add redundancy only for random loss.
- Audio is encrypted per connection, not with the session key. Removing a member means every peer closing its connection to them; see ADR 0004.
- A browser cannot join (needs a trusted certificate or a second media stack), so the browser listener was dropped.
