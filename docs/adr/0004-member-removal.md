# Removing a member: rotate the key and name who is out

Removing someone from a Secure session rotates the session key, and the key-change message (REKEY) itself names the removed device. Each member blocks that device, installs the key and forwards it to the members who joined through it, all in one step. Two earlier designs sent the removal notice separately from the key and both leaked the new key to the removed member under unlucky timing; carrying the id inside REKEY makes the ordering structural. After a key change every connection must prove the new key within 5 seconds or it is closed.

## Consequences

- The host stops sending to the removed member at once; members who never see the notice drop them within 5 seconds.
- A removed person who knows the session password or PIN can rejoin through a member who joined after the removal. The host must change it; the app says so.
- Blocks last only while each app is running.
- Members who joined through the removed person are cut off too and must rejoin.
- A build older than this REKEY format drops its connection on a removal (it fails closed), so all peers in a session need the same build.
- Identity is the device key, never the display name; duplicate names get a short suffix.
