//! Ownership ordering for the Wintun DLL, adapters and sessions.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

/// Implementors must close each handle with its corresponding native API.
pub(crate) trait Lifecycle {
    /// # Safety
    /// `handle` must be a live adapter owned by the caller, with no sessions.
    unsafe fn close_adapter(&self, handle: NonNull<c_void>);
    /// # Safety
    /// `handle` must be a live session owned by the caller.
    unsafe fn end_session(&self, handle: NonNull<c_void>);
}

pub(crate) struct Adapter<A: Lifecycle> {
    pub(crate) api: Arc<A>,
    pub(crate) handle: NonNull<c_void>,
}

impl<A: Lifecycle> Adapter<A> {
    /// # Safety
    /// `handle` must be a newly acquired adapter owned exclusively here.
    pub(crate) unsafe fn from_handle(api: Arc<A>, handle: NonNull<c_void>) -> Arc<Self> {
        Arc::new(Self { api, handle })
    }
}

impl<A: Lifecycle> Drop for Adapter<A> {
    fn drop(&mut self) {
        // SAFETY: the constructor transfers unique ownership; every session
        // holds an Arc and is ended before releasing that Arc.
        unsafe { self.api.close_adapter(self.handle) };
    }
}

pub(crate) struct Session<A: Lifecycle> {
    adapter: Arc<Adapter<A>>,
    handle: NonNull<c_void>,
}

impl<A: Lifecycle> Session<A> {
    /// # Safety
    /// `handle` must be a newly acquired session belonging to `adapter`.
    pub(crate) unsafe fn from_handle(adapter: Arc<Adapter<A>>, handle: NonNull<c_void>) -> Self {
        Self { adapter, handle }
    }

    pub(crate) fn api(&self) -> &A {
        &self.adapter.api
    }

    pub(crate) fn handle(&self) -> NonNull<c_void> {
        self.handle
    }
}

impl<A: Lifecycle> Drop for Session<A> {
    fn drop(&mut self) {
        // SAFETY: constructor transfers unique session ownership, while the
        // adapter and library remain alive through this call.
        unsafe { self.adapter.api.end_session(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake(Arc<Mutex<Vec<&'static str>>>);

    impl Lifecycle for Fake {
        unsafe fn close_adapter(&self, _: NonNull<c_void>) {
            self.0.lock().unwrap().push("adapter");
        }
        unsafe fn end_session(&self, _: NonNull<c_void>) {
            self.0.lock().unwrap().push("session");
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("library");
        }
    }

    #[test]
    fn sessions_end_before_adapter_and_library_even_if_caller_drops_them_first() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let api = Arc::new(Fake(events.clone()));
        let handle = NonNull::dangling();
        // SAFETY: the fake API never dereferences the sentinel handles.
        let adapter = unsafe { Adapter::from_handle(api.clone(), handle) };
        // SAFETY: the fake API never dereferences the sentinel handles.
        let first = unsafe { Session::from_handle(adapter.clone(), handle) };
        // SAFETY: each fake session represents a distinct simulated handle.
        let second = unsafe { Session::from_handle(adapter.clone(), handle) };
        drop(adapter);
        drop(api);
        assert!(events.lock().unwrap().is_empty());
        drop(first);
        assert_eq!(*events.lock().unwrap(), ["session"]);
        drop(second);
        assert_eq!(
            *events.lock().unwrap(),
            ["session", "session", "adapter", "library"]
        );
    }
}
