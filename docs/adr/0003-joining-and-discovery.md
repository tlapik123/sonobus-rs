# How people join a Secure session

A Secure session can be joined by invite link or QR code (`sonobus2://join/...`, carries the session key), by group name + password, or by a short PIN on the local network. There are no open public groups: the legacy public-group list is the main way strangers end up in sessions, and we did not want it in the encrypted protocol.

- **Password and PIN** use SPAKE2 (`spake2` crate, pinned to `=0.5.0-pre.0`, the magic-wormhole variant, not RFC 9382). Guessing is limited online: about 720 guesses a day per session for passwords, five completed wrong guesses per PIN.
- **Name + password over the internet** needs a rendezvous server that both sides can reach. It is self-hostable (`sonobus-server`); we assume no public instance. On a LAN the host's own device serves it, and mDNS (`_sonobus2._udp.local.`) finds sessions.
- **Relays** default to the public n0 relays, which see who connects to whom but never audio. `RelayChoice::{None, N0Default, Custom}` lets a user opt out or self-host. We publish nothing to pkarr/DNS.

## Consequences

- The invite link is a secret. It is printed or stored only when the user asks, and never written to shared setup files, plugin projects or legacy share links.
- Tests never contact n0 relays or any external host (`RelayChoice::None`), so the relayed path is unverified.
