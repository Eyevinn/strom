//! Keep the Intel Quick Sync Video encoders away from the Intel driver bug in #1163.
//!
//! Before 32.0.101.9150, Intel's D3D11 driver for Core Ultra graphics corrupts
//! the process heap (STATUS_HEAP_CORRUPTION) when it refuses a VP9 or AV1
//! encode query. The `qsv` plugin probes sizes it refuses when it loads, so a
//! plain `videotestsrc ! qsvh264enc ! fakesink` crashed about one run in three
//! on Core Ultra, and inside Strom that takes the whole server down.
//!
//! The bug is reported on Core Ultra Series 1 (Meteor Lake) and Series 2
//! (Arrow Lake, Lunar Lake) integrated graphics, and not on Raptor Lake
//! (intel/vpl-gpu-rt#383). So the QSV encoders are demoted only when such an
//! adapter runs a driver older than [`QSV_FIXED_INTEL_DRIVER`], or one whose
//! version cannot be read. Everywhere else they keep their rank.

/// The Intel Quick Sync Video encoders from GStreamer's `qsv` plugin.
pub const QSV_ENCODERS: [&str; 4] = ["qsvh264enc", "qsvh265enc", "qsvvp9enc", "qsvav1enc"];

/// The first Intel graphics driver for Windows whose D3D11 driver no longer
/// corrupts the heap when it refuses an encode query (#1163).
pub const QSV_FIXED_INTEL_DRIVER: [u16; 4] = [32, 0, 101, 9150];

/// Where to get it.
const INTEL_DRIVER_DOWNLOAD: &str =
    "https://www.intel.com/content/www/us/en/download/785597/intel-arc-graphics-windows.html";

/// PCI device ids of the Intel GPUs the #1163 driver bug is reported on, with
/// their family. From the Linux kernel's `include/drm/intel/pciids.h`:
/// `INTEL_MTL_IDS`, `INTEL_ARL_IDS` (H, U and S) and `INTEL_LNL_IDS`.
///
/// Discrete Arc (Alchemist, Battlemage), Panther Lake and Raptor Lake and
/// older are not listed: the bug is not reported on them.
const QSV_AFFECTED_DEVICES: [(u16, &str); 13] = [
    // INTEL_MTL_IDS (Core Ultra Series 1)
    (0x7D40, "Meteor Lake"),
    (0x7D45, "Meteor Lake"),
    (0x7D55, "Meteor Lake"),
    (0x7D60, "Meteor Lake"),
    (0x7DD5, "Meteor Lake"),
    // INTEL_ARL_IDS (Core Ultra Series 2)
    (0x7D51, "Arrow Lake"),
    (0x7DD1, "Arrow Lake"),
    (0x7D41, "Arrow Lake"),
    (0x7D67, "Arrow Lake"),
    (0xB640, "Arrow Lake"),
    // INTEL_LNL_IDS (Core Ultra Series 2)
    (0x6420, "Lunar Lake"),
    (0x64A0, "Lunar Lake"),
    (0x64B0, "Lunar Lake"),
];

/// The GPU family of PCI `device_id` when it is one the #1163 driver bug is
/// reported on, `None` otherwise.
pub fn qsv_affected_family(device_id: u32) -> Option<&'static str> {
    let id = u16::try_from(device_id).ok()?;
    QSV_AFFECTED_DEVICES
        .iter()
        .find(|(affected, _)| *affected == id)
        .map(|(_, family)| *family)
}

/// True when PCI `device_id` is an Intel GPU the #1163 driver bug is reported on.
pub fn qsv_affected(device_id: u32) -> bool {
    qsv_affected_family(device_id).is_some()
}

/// True when Intel driver `version` has the fix for #1163. `None` (its
/// version could not be read) counts as not fixed.
pub fn intel_driver_has_qsv_fix(version: Option<[u16; 4]>) -> bool {
    version.is_some_and(|v| v >= QSV_FIXED_INTEL_DRIVER)
}

/// An Intel display adapter, as DXGI reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntelAdapter {
    /// PCI device id.
    pub device_id: u32,
    /// User-mode driver version, `None` when it cannot be read.
    pub driver: Option<[u16; 4]>,
}

/// The first adapter that puts the QSV encoders at risk of #1163: an affected
/// GPU whose driver is older than [`QSV_FIXED_INTEL_DRIVER`] or unknown.
/// `None` means the QSV encoders can keep their rank.
pub fn qsv_at_risk(adapters: &[IntelAdapter]) -> Option<&IntelAdapter> {
    adapters
        .iter()
        .find(|a| qsv_affected(a.device_id) && !intel_driver_has_qsv_fix(a.driver))
}

fn show_version(version: Option<[u16; 4]>) -> String {
    version
        .map(|v| format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Drop the Intel Quick Sync Video encoders' rank to NONE on Windows when an
/// Intel GPU that has the #1163 driver bug runs a driver older than
/// [`QSV_FIXED_INTEL_DRIVER`] (or one whose version cannot be read), so nothing
/// picks them: neither the Video Encoder block, which skips an encoder of rank
/// NONE, nor `webrtcsink`'s encoder discovery in WHEP Output.
///
/// On other GPUs, with a fixed driver, or on other platforms, the encoders keep
/// their rank. Call after `gst::init()` and before `register_static_plugins()`:
/// `webrtcsink` reads encoder ranks when it registers.
pub fn demote_qsv_encoders() {
    use gstreamer::prelude::PluginFeatureExtManual;

    if !cfg!(windows) {
        return;
    }
    let adapters = intel_adapters();
    for adapter in &adapters {
        tracing::debug!(
            "Intel adapter 0x{:04X} ({}), driver {}",
            adapter.device_id,
            qsv_affected_family(adapter.device_id).unwrap_or("not affected by #1163"),
            show_version(adapter.driver)
        );
    }
    let Some(risk) = qsv_at_risk(&adapters) else {
        if adapters.is_empty() {
            tracing::info!("No Intel graphics adapter: QSV encoders keep their rank");
        } else if adapters.iter().any(|a| qsv_affected(a.device_id)) {
            tracing::info!(
                "Intel graphics driver has the fix for #1163: QSV encoders enabled ({})",
                adapters
                    .iter()
                    .map(|a| format!("0x{:04X} {}", a.device_id, show_version(a.driver)))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        } else {
            tracing::info!(
                "Intel graphics adapter not affected by #1163: QSV encoders keep their rank"
            );
        }
        return;
    };
    let installed: Vec<&str> = QSV_ENCODERS
        .into_iter()
        .filter(|name| {
            gstreamer::ElementFactory::find(name)
                .map(|factory| {
                    let ranked = factory.rank() != gstreamer::Rank::NONE;
                    factory.set_rank(gstreamer::Rank::NONE);
                    ranked
                })
                .unwrap_or(false)
        })
        .collect();
    if !installed.is_empty() {
        let [a, b, c, d] = QSV_FIXED_INTEL_DRIVER;
        tracing::warn!(
            "Intel QSV encoders disabled ({}): Intel graphics driver {} on {} graphics \
             (0x{:04X}) corrupts the heap when QSV probes its encoders (#1163). \
             Install {}.{}.{}.{} or later to use them: {}",
            installed.join(", "),
            show_version(risk.driver),
            qsv_affected_family(risk.device_id).unwrap_or("Intel"),
            risk.device_id,
            a,
            b,
            c,
            d,
            INTEL_DRIVER_DOWNLOAD
        );
    }
}

/// The Intel display adapters, from DXGI.
#[cfg(windows)]
pub fn intel_adapters() -> Vec<IntelAdapter> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1};

    const INTEL: u32 = 0x8086;
    let mut adapters = Vec::new();
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(factory) => factory,
        Err(_) => return adapters,
    };
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if desc.VendorId != INTEL {
            continue;
        }
        // The user-mode driver version, packed as four 16-bit parts.
        let driver = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
            .ok()
            .map(|packed| {
                [
                    (packed >> 48) as u16,
                    (packed >> 32) as u16,
                    (packed >> 16) as u16,
                    packed as u16,
                ]
            });
        adapters.push(IntelAdapter {
            device_id: desc.DeviceId,
            driver,
        });
    }
    adapters
}

#[cfg(not(windows))]
pub fn intel_adapters() -> Vec<IntelAdapter> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: Option<[u16; 4]> = Some([32, 0, 101, 8991]);
    const FIXED: Option<[u16; 4]> = Some(QSV_FIXED_INTEL_DRIVER);
    /// Core Ultra 7 165H (Meteor Lake), the #1163 repro host's GPU.
    const MTL: u32 = 0x7D55;
    /// Core i7-1360P (Raptor Lake-P), not affected.
    const RPL: u32 = 0xA7A0;

    fn adapter(device_id: u32, driver: Option<[u16; 4]>) -> IntelAdapter {
        IntelAdapter { device_id, driver }
    }

    fn demotes(adapters: &[IntelAdapter]) -> bool {
        qsv_at_risk(adapters).is_some()
    }

    #[test]
    fn affected_gpu_with_old_driver_demotes() {
        assert!(demotes(&[adapter(MTL, OLD)]));
    }

    #[test]
    fn affected_gpu_with_unknown_driver_demotes() {
        assert!(demotes(&[adapter(MTL, None)]));
    }

    #[test]
    fn affected_gpu_with_fixed_driver_keeps() {
        assert!(!demotes(&[adapter(MTL, FIXED)]));
        assert!(!demotes(&[adapter(MTL, Some([33, 0, 0, 0]))]));
    }

    #[test]
    fn unaffected_gpu_with_old_driver_keeps() {
        assert!(!demotes(&[adapter(RPL, OLD)]));
    }

    #[test]
    fn unaffected_gpu_with_unknown_driver_keeps() {
        assert!(!demotes(&[adapter(RPL, None)]));
    }

    #[test]
    fn no_intel_adapter_keeps() {
        assert!(!demotes(&[]));
    }

    #[test]
    fn any_affected_adapter_at_risk_demotes() {
        // Raptor Lake on an old driver beside a fixed Meteor Lake: keep.
        assert!(!demotes(&[adapter(RPL, OLD), adapter(MTL, FIXED)]));
        // A fixed affected adapter beside one at risk: demote, and name it.
        let lnl = adapter(0x64A0, OLD);
        let adapters = [adapter(RPL, OLD), adapter(MTL, FIXED), lnl];
        assert_eq!(qsv_at_risk(&adapters), Some(&lnl));
    }

    #[test]
    fn reported_core_ultra_gpus_are_affected() {
        // 155H/165H/185H, 285H, 265K, 266V (intel/vpl-gpu-rt#383).
        assert_eq!(qsv_affected_family(0x7D55), Some("Meteor Lake"));
        assert_eq!(qsv_affected_family(0x7D51), Some("Arrow Lake"));
        assert_eq!(qsv_affected_family(0x7D67), Some("Arrow Lake"));
        assert_eq!(qsv_affected_family(0x64A0), Some("Lunar Lake"));
    }

    #[test]
    fn other_intel_gpus_are_not_affected() {
        // Raptor Lake-P/-S, Alder Lake-P, Arc A770, Arc B580, Panther Lake,
        // and an id that does not fit in 16 bits.
        for id in [0xA7A0, 0xA780, 0x46A6, 0x56A0, 0xE20B, 0xB080, 0x1_7D55] {
            assert!(!qsv_affected(id), "0x{id:X} counted as affected");
        }
    }

    #[test]
    fn affected_device_ids_are_unique() {
        for (i, (id, _)) in QSV_AFFECTED_DEVICES.iter().enumerate() {
            assert!(
                QSV_AFFECTED_DEVICES[i + 1..].iter().all(|(o, _)| o != id),
                "0x{id:04X} listed twice"
            );
        }
    }

    /// QSV is only safe from Intel driver 32.0.101.9150 on. An unknown
    /// version is not.
    #[test]
    fn qsv_needs_intel_driver_9150_or_later() {
        use intel_driver_has_qsv_fix as fixed;
        assert!(!fixed(None));
        assert!(!fixed(Some([32, 0, 101, 8991])));
        assert!(!fixed(Some([32, 0, 101, 8247])));
        assert!(!fixed(Some([31, 0, 101, 9999])));
        assert!(fixed(Some([32, 0, 101, 9150])));
        assert!(fixed(Some([32, 0, 101, 9151])));
        assert!(fixed(Some([32, 0, 102, 1])));
        assert!(fixed(Some([33, 0, 0, 0])));
    }
}
