//! veth pair management (future work).
//!
//! Requires `CAP_SYS_ADMIN` in the host namespace, so it belongs to the
//! privileged helper phase — not the rootless slice. Will create a
//! `cmux-veth-<ws-id>` pair, move one end into the workspace netns, and assign
//! addresses. Unused for now.
