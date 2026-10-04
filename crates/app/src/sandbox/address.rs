//! Workspace IP address allocator (future work).
//!
//! Reserved for the privileged veth phase: a `10.88.0.0/24` allocator that
//! hands out `10.88.0.2`, `10.88.0.3`, … and reuses freed addresses without
//! collisions. Not reachable without a privileged helper, so it is unused in
//! the rootless slice.
