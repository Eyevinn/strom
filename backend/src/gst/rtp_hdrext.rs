//! Disable RTP header-extension aggregation on every depayloader in a pipeline.
//!
//! Works around an unfixed abort in `GstRTPBaseDepayload`
//! ([gstreamer#5057](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/5057)):
//!
//! ```text
//! gstrtpbasedepayload.c:942:gst_rtp_base_depayload_handle_buffer:
//! 'gst_buffer_list_length (priv->hdrext_buffers) == 0' should be TRUE
//! ```
//!
//! Since 1.24 the base class caches the RTP header of every packet feeding the
//! output buffer it is assembling, clearing the cache only when the subclass
//! pushes or flushes. `gst_rtp_base_depayload_delayed()` means "this packet's
//! header belongs to the next output buffer", and the base class asserts the
//! cache is empty when that happens. `rtph264depay` breaks the invariant: an
//! interrupted fragmentation unit calls `delayed()` and then
//! `finish_fragmentation_unit()`, which in access-unit mode can absorb the
//! truncated NAL without producing an output buffer. Nothing is pushed, the
//! cache is still populated, and the process aborts — taking every unrelated
//! flow on the server with it, since `g_assert_true` is not defusable.
//!
//! Turning aggregation off restores the pre-1.24 behaviour: header extensions
//! are read from the current packet instead of accumulated. Strom reads no
//! header-extension metadata, and the extensions that matter for transport
//! (transport-cc, abs-send-time, mid) are consumed by `webrtcbin` well upstream
//! of any depayloader, so nothing is lost.
//!
//! There is no GObject property for this — the C setter is the only switch, and
//! it is `Since: 1.24`. Binding it normally would raise the workspace build
//! floor from 1.22, which would break the default install on Debian 12 and the
//! default ARM64 cross-compile target (Raspberry Pi OS 12), both of which ship
//! GStreamer 1.22. Resolving the symbol at runtime keeps one binary working on
//! both: below 1.24 the lookup fails and we do nothing, which is correct
//! because aggregation — and therefore the bug — does not exist there.

use gstreamer as gst;
use gstreamer::glib::translate::ToGlibPtr;
use gstreamer::prelude::*;
use std::ffi::c_void;
use std::sync::OnceLock;
use tracing::{debug, info};

const SETTER: &str = "gst_rtp_base_depayload_set_aggregate_hdrext_enabled";
const GETTER: &str = "gst_rtp_base_depayload_is_aggregate_hdrext_enabled";

/// `void (GstRTPBaseDepayload *depayload, gboolean enable)`
type SetAggregateFn = unsafe extern "C" fn(*mut gst::ffi::GstElement, i32);
/// `gboolean (GstRTPBaseDepayload *depayload)`
type IsAggregateFn = unsafe extern "C" fn(*mut gst::ffi::GstElement) -> i32;

/// Resolve a symbol from the already-loaded GStreamer RTP library.
///
/// `gstreamer-rtp-sys` is a build dependency of the binary, so `libgstrtp-1.0`
/// is loaded and its symbols are in scope before this runs.
#[cfg(unix)]
fn lookup(name: &str) -> Option<*mut c_void> {
    let cname = std::ffi::CString::new(name).ok()?;
    // SAFETY: `cname` is a valid NUL-terminated string for the duration of the
    // call. `dlsym` with RTLD_DEFAULT only reads the global symbol table and
    // returns a plain address or NULL; it does not take ownership of anything.
    let addr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) };
    (!addr.is_null()).then_some(addr)
}

#[cfg(windows)]
fn lookup(name: &str) -> Option<*mut c_void> {
    const FROM_ADDRESS: u32 = 0x0000_0004;
    const UNCHANGED_REFCOUNT: u32 = 0x0000_0002;

    extern "system" {
        fn GetModuleHandleExA(flags: u32, name: *const i8, module: *mut *mut c_void) -> i32;
        fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
    }

    let cname = std::ffi::CString::new(name).ok()?;
    // Anchor on a symbol we already link from the same DLL rather than
    // hardcoding its filename, which varies between GStreamer builds.
    let anchor = gstreamer_rtp::ffi::gst_rtp_base_depayload_get_type as *const c_void;
    let mut module: *mut c_void = std::ptr::null_mut();

    // SAFETY: `anchor` is the address of a function in the loaded RTP DLL, and
    // `module` is a valid out-pointer. UNCHANGED_REFCOUNT means we take no
    // reference, so there is nothing to release. `GetProcAddress` reads the
    // module's export table and returns a plain address or NULL.
    unsafe {
        if GetModuleHandleExA(
            FROM_ADDRESS | UNCHANGED_REFCOUNT,
            anchor as *const i8,
            &mut module,
        ) == 0
        {
            return None;
        }
        let addr = GetProcAddress(module, cname.as_ptr());
        (!addr.is_null()).then_some(addr)
    }
}

#[cfg(not(any(unix, windows)))]
fn lookup(_name: &str) -> Option<*mut c_void> {
    None
}

fn setter() -> Option<SetAggregateFn> {
    static SYM: OnceLock<Option<usize>> = OnceLock::new();
    let addr = SYM.get_or_init(|| {
        let found = lookup(SETTER).map(|p| p as usize);
        match found {
            Some(_) => debug!("{SETTER} resolved; hdrext aggregation will be disabled"),
            None => info!(
                "{SETTER} not found (GStreamer < 1.24) - header extension aggregation \
                 does not exist on this version, nothing to disable"
            ),
        }
        found
    });
    let addr = (*addr)?;
    // SAFETY: the address came from resolving `SETTER` in the loaded RTP
    // library, whose signature has been stable since 1.24.
    Some(unsafe { std::mem::transmute::<usize, SetAggregateFn>(addr) })
}

fn getter() -> Option<IsAggregateFn> {
    static SYM: OnceLock<Option<usize>> = OnceLock::new();
    let addr = (*SYM.get_or_init(|| lookup(GETTER).map(|p| p as usize)))?;
    // SAFETY: as above, for the matching getter.
    Some(unsafe { std::mem::transmute::<usize, IsAggregateFn>(addr) })
}

/// Turn aggregation off on one depayloader. No-op below GStreamer 1.24.
fn disable_on(depay: &gstreamer_rtp::RTPBaseDepayload) {
    let Some(set) = setter() else { return };
    let ptr: *mut gst::ffi::GstElement = depay.upcast_ref::<gst::Element>().to_glib_none().0;
    // SAFETY: `ptr` is a live GstRTPBaseDepayload borrowed for this call — the
    // downcast to `RTPBaseDepayload` guarantees the type, and `depay` keeps it
    // alive. The setter only writes a boolean field and clears an internal
    // buffer list.
    unsafe { set(ptr, 0) };
}

/// Read aggregation state back. `None` below GStreamer 1.24.
pub fn is_enabled(depay: &gstreamer_rtp::RTPBaseDepayload) -> Option<bool> {
    let get = getter()?;
    let ptr: *mut gst::ffi::GstElement = depay.upcast_ref::<gst::Element>().to_glib_none().0;
    // SAFETY: as in `disable_on`; the getter only reads a boolean field.
    Some(unsafe { get(ptr) } != 0)
}

/// Whether this GStreamer build has the aggregation switch at all.
pub fn is_supported() -> bool {
    setter().is_some()
}

/// Disable header-extension aggregation on every depayloader the pipeline ever
/// contains, including ones `decodebin` autoplugs and ones a user places by
/// hand in a flow.
///
/// Install before the pipeline leaves NULL so no depayloader is missed.
pub fn install(pipeline: &gst::Pipeline) {
    if !is_supported() {
        return;
    }

    let bin = pipeline.upcast_ref::<gst::Bin>();

    // Elements already present (a depayloader placed directly in a flow is
    // built before start()); `deep-element-added` only fires for later ones.
    for element in bin.iterate_recurse().into_iter().flatten() {
        if let Some(depay) = element.downcast_ref::<gstreamer_rtp::RTPBaseDepayload>() {
            disable_on(depay);
        }
    }

    // NOTE: this closure captures nothing at all - no pipeline, no element, no
    // map - so it cannot create a reference cycle that keeps the pipeline
    // alive. Keep it that way.
    bin.connect("deep-element-added", false, move |args| {
        let added: gst::Element = args.get(2)?.get().ok()?;
        if let Some(depay) = added.downcast_ref::<gstreamer_rtp::RTPBaseDepayload>() {
            debug!(
                "Disabling RTP header extension aggregation on {} (gstreamer#5057)",
                added.name()
            );
            disable_on(depay);
        }
        None
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        let _ = gst::init();
    }

    fn depay() -> gstreamer_rtp::RTPBaseDepayload {
        gst::ElementFactory::make("rtph264depay")
            .build()
            .expect("rtph264depay is in gstreamer1.0-plugins-good, installed in CI")
            .downcast::<gstreamer_rtp::RTPBaseDepayload>()
            .expect("rtph264depay derives from GstRTPBaseDepayload")
    }

    /// The whole workaround hinges on resolving the symbol at runtime. If this
    /// fails on a >= 1.24 build, `install()` silently does nothing.
    #[test]
    fn symbol_resolves_on_modern_gstreamer() {
        init();
        let (major, minor, ..) = gst::version();
        if (major, minor) >= (1, 24) {
            assert!(
                is_supported(),
                "GStreamer {major}.{minor} has the aggregation API but the runtime \
                 lookup failed - the workaround would silently no-op"
            );
        }
    }

    /// `rtph264depay` opts into aggregation in its _init, so this documents the
    /// unpatched default and proves the getter reads real state.
    #[test]
    fn rtph264depay_enables_aggregation_by_default() {
        init();
        if !is_supported() {
            return;
        }
        assert_eq!(is_enabled(&depay()), Some(true));
    }

    /// Guard: a depayloader already in the pipeline when `install()` runs.
    /// Fails if the `iterate_recurse` sweep is removed.
    #[test]
    fn install_disables_aggregation_on_existing_depayloader() {
        init();
        if !is_supported() {
            return;
        }
        let pipeline = gst::Pipeline::new();
        let d = depay();
        pipeline.add(&d).unwrap();
        assert_eq!(is_enabled(&d), Some(true), "precondition");

        install(&pipeline);

        assert_eq!(is_enabled(&d), Some(false));
    }

    /// Guard: a depayloader added to a nested bin *after* `install()` - the
    /// shape `decodebin` produces when it autoplugs one. Fails if the
    /// `deep-element-added` handler is removed.
    #[test]
    fn install_disables_aggregation_on_later_nested_depayloader() {
        init();
        if !is_supported() {
            return;
        }
        let pipeline = gst::Pipeline::new();
        let inner = gst::Bin::builder().name("inner").build();
        pipeline.add(&inner).unwrap();

        install(&pipeline);

        // Added after install, one level down - only deep-element-added sees it.
        let d = depay();
        inner.add(&d).unwrap();

        assert_eq!(is_enabled(&d), Some(false));
    }
}
