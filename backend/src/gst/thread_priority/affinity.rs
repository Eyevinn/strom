//! The CPU affinity of the calling streaming thread, and putting it back.
//!
//! Like priority, affinity sticks to a pooled thread when it goes back to
//! GStreamer's pool. Before the first pin the thread's own mask is saved, and
//! [`restore_current_thread`] puts it back wherever scheduling is restored: as
//! the thread leaves, or as the next pipeline picks it up. Only Linux pins
//! threads; elsewhere both are no-ops.

#[cfg(target_os = "linux")]
thread_local! {
    /// The calling thread's affinity from before this module first pinned it.
    /// `None` means this module has not pinned the thread.
    static SAVED: std::cell::RefCell<Option<libc::cpu_set_t>> = const { std::cell::RefCell::new(None) };
}

/// Pin the calling thread to `cpus`. `Ok(false)` where this platform does not
/// pin threads.
#[cfg(target_os = "linux")]
pub(super) fn pin_current_thread(cpus: &[usize]) -> Result<bool, String> {
    // Refuse to pin a thread whose mask we could not save: it would go back
    // to the pool pinned with nothing to restore it from.
    SAVED.with(|saved| {
        let mut saved = saved.borrow_mut();
        if saved.is_none() {
            *saved = Some(current_mask()?);
        }
        Ok::<(), String>(())
    })?;
    // SAFETY: an all-zero cpu_set_t is the empty set.
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &cpu in cpus {
        // SAFETY: CPU_SET bounds-checks `cpu` against the set's size.
        unsafe { libc::CPU_SET(cpu, &mut mask) };
    }
    apply_mask(&mask)?;
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn pin_current_thread(_cpus: &[usize]) -> Result<bool, String> {
    Ok(false)
}

/// Undo [`pin_current_thread`] on the calling thread. A thread this module
/// never pinned is left alone.
#[cfg(target_os = "linux")]
pub(super) fn restore_current_thread() -> Result<(), String> {
    let Some(saved) = SAVED.with(|saved| saved.borrow_mut().take()) else {
        return Ok(());
    };
    apply_mask(&saved).inspect_err(|_| {
        // Keep it, so the next Leave or Enter on this thread retries.
        SAVED.with(|s| *s.borrow_mut() = Some(saved));
    })
}

#[cfg(not(target_os = "linux"))]
pub(super) fn restore_current_thread() -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn current_mask() -> Result<libc::cpu_set_t, String> {
    // SAFETY: an all-zero cpu_set_t is valid, and the kernel fills it in.
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: pid 0 is the calling thread; the size matches `mask`.
    let ret =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut mask) };
    if ret == 0 {
        Ok(mask)
    } else {
        Err(format!(
            "sched_getaffinity failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(target_os = "linux")]
fn apply_mask(mask: &libc::cpu_set_t) -> Result<(), String> {
    // SAFETY: pid 0 is the calling thread; the size matches `mask`.
    let ret = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), mask) };
    if ret == 0 {
        Ok(())
    } else {
        Err(format!(
            "sched_setaffinity failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

/// The calling thread's CPU mask, for tests.
#[cfg(all(test, target_os = "linux"))]
pub(super) fn current_cpus() -> Vec<usize> {
    let mask = current_mask().unwrap();
    // SAFETY: CPU_ISSET bounds-checks the index.
    (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &mask) })
        .collect()
}
