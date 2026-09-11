//! Owned handles to the threads the CPU sampler reads.
//!
//! The registry keeps an identifier for a GStreamer streaming thread and hands
//! it to [`crate::system_monitor::ThreadCpuSampler`] later, possibly after that
//! thread has exited. On Linux and Windows the identifier is an integer that is
//! merely stale once the thread is gone. On macOS it is a mach port name, and a
//! bare name is not safe to keep.
//!
//! `pthread_mach_thread_np()` returns the calling thread's port *name* without
//! taking a user reference on it. When the thread exits the name is freed, and
//! the kernel may hand that same name to an unrelated port. Passing a recycled
//! name to `thread_info()` is not merely wrong: if the name now belongs to a
//! *guarded* port — libdispatch and XPC guard theirs — the kernel raises
//! `EXC_GUARD` and kills the process. There is no error return to check.
//!
//! [`ThreadHandle`] holds a reference instead: `mach_thread_self()` returns the
//! same name *with* a send right, and the name stays bound to that thread's
//! port until the matching `mach_port_deallocate()`. That deallocate happens in
//! `Drop`, so every way a handle can go away — a single unregister, a whole
//! flow's entries, the registry itself, one registration overwriting another —
//! releases the right exactly once. An `Arc` holds it, so a handle cloned out
//! of the registry (as `ThreadRegistry::get_all` does on every sampling tick)
//! keeps the port alive for as long as the sampler holds the clone, even if the
//! entry is unregistered concurrently.
//!
//! Invariant: a mach port name is never passed to `thread_info()` unless a live
//! [`ThreadHandle`] guarantees the name still refers to the same thread.

#[cfg(target_os = "macos")]
use std::sync::Arc;

/// An identifier for a thread, plus whatever ownership is needed to keep that
/// identifier meaningful after the thread has exited.
///
/// Construct one with [`ThreadHandle::current`], on the thread being
/// registered. There is deliberately no constructor from a raw integer: on
/// macOS a bare port name carries no reference, which is the bug this type
/// exists to prevent.
#[derive(Clone, Debug)]
pub struct ThreadHandle {
    /// Stable identifier used as the registry key and reported to the UI.
    id: u64,
    /// macOS only: the send right that keeps `id` naming this thread's port.
    #[cfg(target_os = "macos")]
    port: Arc<MachThreadPort>,
}

impl ThreadHandle {
    /// Capture a handle to the calling thread.
    ///
    /// Must be called on the thread being registered — on macOS the port
    /// reference can only be taken from the thread itself, which is also what
    /// makes the capture race-free: the thread is alive by definition.
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            // gettid(), not pthread_self(): /proc/{pid}/task/{tid} is keyed by
            // the kernel TID.
            let id = unsafe { libc::syscall(libc::SYS_gettid) as u64 };
            Self { id }
        }

        #[cfg(target_os = "macos")]
        {
            // SAFETY: mach_thread_self() is callable from any thread and
            // returns a send right to the caller's own thread port, with a
            // user reference that MachThreadPort now owns and releases on drop.
            let port = unsafe { sys::mach_thread_self() };
            Self {
                id: port as u64,
                port: Arc::new(MachThreadPort(port)),
            }
        }

        #[cfg(target_os = "windows")]
        {
            extern "system" {
                fn GetCurrentThreadId() -> u32;
            }
            let id = unsafe { GetCurrentThreadId() as u64 };
            Self { id }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Self { id: 0 }
        }
    }

    /// The identifier used as the registry key and reported to the UI.
    ///
    /// This is only a number. On macOS, do not pass it to `thread_info()` —
    /// use [`ThreadHandle::mach_port`], which is reachable only while this
    /// handle (and therefore the port reference) is alive.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The mach port name, valid for as long as `self` is alive.
    #[cfg(target_os = "macos")]
    pub(crate) fn mach_port(&self) -> libc::mach_port_t {
        self.port.0
    }
}

/// A user reference on a thread's mach port, released on drop.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MachThreadPort(libc::mach_port_t);

#[cfg(target_os = "macos")]
impl Drop for MachThreadPort {
    fn drop(&mut self) {
        // SAFETY: releases exactly the reference mach_thread_self() took in
        // ThreadHandle::current(). The reference count returns to what it was
        // before, never to zero, so this cannot trip the pinned-port guard.
        // mach_port_deallocate accepts both a send right and the dead name a
        // send right becomes, so the thread's liveness does not matter here.
        unsafe {
            sys::mach_port_deallocate(sys::mach_task_self_, self.0);
        }
    }
}

/// Mach symbols from `<mach/mach_port.h>` and `<mach/mach_init.h>`.
///
/// `libc` deprecated its mach surface in favour of the `mach2` crate and does
/// not expose `mach_port_deallocate` at all; declaring the handful we need
/// keeps that dependency out. `ipc_space_t` and `mach_port_name_t` are both
/// `mach_port_t`, and `mach_task_self()` is a C macro over the global below.
#[cfg(target_os = "macos")]
mod sys {
    extern "C" {
        pub fn mach_thread_self() -> libc::mach_port_t;

        pub fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;

        #[cfg(test)]
        pub fn mach_port_type(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
            ptype: *mut libc::natural_t,
        ) -> libc::kern_return_t;

        pub static mach_task_self_: libc::mach_port_t;
    }
}

/// Whether `name` is still allocated in this task's IPC space.
///
/// `false` means the name has been freed and the kernel may hand it to any
/// other port — which is exactly the state a cached name must never be in
/// while something is still willing to call `thread_info()` on it.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn mach_port_name_is_allocated(name: libc::mach_port_t) -> bool {
    let mut ptype: libc::natural_t = 0;
    // SAFETY: `ptype` is a valid out parameter; the call only inspects `name`.
    let kr = unsafe { sys::mach_port_type(sys::mach_task_self_, name, &mut ptype) };
    kr == libc::KERN_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle captured on another thread must outlive that thread.
    ///
    /// On macOS this is the crash fix: without the retained send right the
    /// port name is freed the moment the thread exits and can be recycled to
    /// an unrelated — possibly guarded — port, so a later `thread_info()` on
    /// the cached name can take down the process.
    #[test]
    #[cfg(target_os = "macos")]
    fn handle_keeps_the_port_name_allocated_after_the_thread_exits() {
        let handle = std::thread::spawn(ThreadHandle::current).join().unwrap();
        let name = handle.mach_port();

        assert!(
            mach_port_name_is_allocated(name),
            "port name {:#x} was freed while a ThreadHandle still held it; it can now be recycled",
            name
        );

        drop(handle);

        assert!(
            !mach_port_name_is_allocated(name),
            "port name {:#x} is still allocated after the last handle was dropped: the reference leaked",
            name
        );
    }

    /// Cloning a handle must not double-release the reference, and the port
    /// must survive until the *last* clone is gone. This is what makes it safe
    /// for the sampler to work from a snapshot taken out of the registry.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_clone_keeps_the_port_alive_after_the_original_is_dropped() {
        let handle = std::thread::spawn(ThreadHandle::current).join().unwrap();
        let name = handle.mach_port();
        let clone = handle.clone();

        drop(handle);
        assert!(
            mach_port_name_is_allocated(name),
            "dropping one clone released the shared port reference"
        );

        drop(clone);
        assert!(!mach_port_name_is_allocated(name));
    }

    /// The calling thread's own handle must name the same port the platform's
    /// non-owning accessor reports, so the identifier the UI shows and the
    /// registry keys on does not change meaning with this indirection.
    #[test]
    #[cfg(target_os = "macos")]
    fn current_matches_the_non_owning_port_name() {
        let handle = ThreadHandle::current();
        let bare = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        assert_eq!(handle.mach_port(), bare);
        assert_eq!(handle.id(), bare as u64);
    }

    #[test]
    fn ids_are_distinct_per_thread() {
        let a = ThreadHandle::current();
        let b = std::thread::spawn(ThreadHandle::current).join().unwrap();
        assert_ne!(a.id(), b.id());
    }
}
