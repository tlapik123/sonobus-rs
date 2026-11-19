# Two protocols, chosen per session

SonoBus users are on the stock AOO protocol, which is unencrypted and uses MD5 passwords, so we need it for interoperability but cannot build new features on it. Each session is therefore either **Compatible** (stock SonoBus 1.x, byte-compatible) or **Secure** (our own protocol, this app only); the two never mix in one session. The engine keeps both behind one shared layer (`sonobus-engine/src/proto.rs`, `legacy/`, `modern/`).

## Consequences

- Every feature that touches the network is implemented twice or is Secure-only.
- Compatible sessions are labelled as not encrypted in the UI; that wording is deliberate.
