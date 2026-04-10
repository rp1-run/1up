#[cfg(unix)]
pub mod ipc;

#[cfg(unix)]
pub mod lifecycle;
#[cfg(not(unix))]
#[path = "lifecycle_stub.rs"]
pub mod lifecycle;

pub mod registry;

#[cfg(unix)]
pub mod search_service;
#[cfg(not(unix))]
#[path = "search_service_stub.rs"]
pub mod search_service;

#[cfg(unix)]
pub mod watcher;

#[cfg(unix)]
pub mod worker;
#[cfg(not(unix))]
#[path = "worker_stub.rs"]
pub mod worker;
