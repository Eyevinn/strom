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
//!
//! Policy: [`QSV_AFFECTED_DEVICES`] lists the GPUs the bug is known on. It is
//! deliberately not inverted into "demote every GPU not known to be safe": an
//! unknown GPU keeps its QSV encoders. When another GPU is reported affected,
//! its PCI ids are added to the list. Until then the operator can force the
//! decision with [`QSV_ENV`].

/// Environment variable that overrides the detection: `auto` (or unset),
/// `on`/`enable` (never demote), `off`/`disable` (always demote, on every
/// platform).
pub const QSV_ENV: &str = "STROM_QSV";

/// The Intel Quick Sync Video encoders from GStreamer's `qsv` plugin, for its
/// first device. Further devices register `qsvh264device1enc` and so on, which
/// [`qsv_encoder_features`] finds in the registry.
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
/// older are not listed: the bug is not reported on them. This is a list of
/// known affected GPUs, not of known safe ones (see the module docs).
pub const QSV_AFFECTED_DEVICES: [(u16, &str); 13] = [
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

/// Something the adapter scan could not read. Unknown hardware keeps its QSV
/// encoders, but the operator is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanIssue {
    /// DXGI could not be opened, so no adapter was seen at all.
    Dxgi(String),
    /// An adapter's description could not be read, so its vendor is unknown.
    AdapterDesc { index: u32, error: String },
}

impl std::fmt::Display for ScanIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dxgi(error) => write!(f, "could not open DXGI: {error}"),
            Self::AdapterDesc { index, error } => {
                write!(f, "could not read display adapter {index}: {error}")
            }
        }
    }
}

/// What a DXGI scan found: the Intel adapters, and what it could not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterScan {
    pub intel: Vec<IntelAdapter>,
    pub issues: Vec<ScanIssue>,
}

/// The first adapter that puts the QSV encoders at risk of #1163: an affected
/// GPU whose driver is older than [`QSV_FIXED_INTEL_DRIVER`] or unknown.
/// `None` means the QSV encoders can keep their rank.
pub fn qsv_at_risk(adapters: &[IntelAdapter]) -> Option<&IntelAdapter> {
    adapters
        .iter()
        .find(|a| qsv_affected_family(a.device_id).is_some() && !intel_driver_has_qsv_fix(a.driver))
}

/// The operator's choice in [`QSV_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QsvMode {
    /// Demote only on an affected GPU with an old driver (the default).
    Auto,
    /// Never demote.
    On,
    /// Always demote, on every platform.
    Off,
}

/// Parse a [`QSV_ENV`] value. Unset or blank is [`QsvMode::Auto`]. Case and
/// surrounding spaces are ignored. An unknown value is returned as the error.
pub fn parse_qsv_mode(value: Option<&str>) -> Result<QsvMode, String> {
    let Some(raw) = value else {
        return Ok(QsvMode::Auto);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(QsvMode::Auto),
        "on" | "enable" => Ok(QsvMode::On),
        "off" | "disable" => Ok(QsvMode::Off),
        _ => Err(raw.to_string()),
    }
}

/// [`QSV_ENV`] from the environment. An invalid value warns and counts as
/// [`QsvMode::Auto`].
pub fn qsv_mode_from_env() -> QsvMode {
    let raw = std::env::var(QSV_ENV).ok();
    parse_qsv_mode(raw.as_deref()).unwrap_or_else(|invalid| {
        tracing::warn!(
            "Ignoring {}={:?}: expected auto, on/enable or off/disable. Using auto",
            QSV_ENV,
            invalid
        );
        QsvMode::Auto
    })
}

/// True when an element factory of class `klass` is an encoder.
fn is_encoder_klass(klass: &str) -> bool {
    klass.split('/').any(|part| part == "Encoder")
}

/// Every QSV encoder feature in the registry: [`QSV_ENCODERS`] and the
/// per-device features (`qsvh264device1enc`, ...) of the `qsv` plugin.
///
/// Reads registry metadata only. Neither listing a plugin's features nor
/// reading a factory's class loads the plugin, and loading it is what
/// triggers #1163.
pub fn qsv_encoder_features() -> Vec<gstreamer::ElementFactory> {
    use gstreamer::prelude::*;

    let mut factories: Vec<gstreamer::ElementFactory> = gstreamer::Registry::get()
        .features_by_plugin("qsv")
        .into_iter()
        .filter_map(|feature| feature.downcast::<gstreamer::ElementFactory>().ok())
        .filter(|factory| is_encoder_klass(factory.klass()))
        .collect();
    for name in QSV_ENCODERS {
        if let Some(factory) = gstreamer::ElementFactory::find(name) {
            if !factories.contains(&factory) {
                factories.push(factory);
            }
        }
    }
    factories
}

fn show_version(version: Option<[u16; 4]>) -> String {
    version
        .map(|v| format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Drop the rank of every Intel Quick Sync Video encoder to NONE when an Intel
/// GPU that has the #1163 driver bug runs a driver older than
/// [`QSV_FIXED_INTEL_DRIVER`] (or one whose version cannot be read), so nothing
/// picks them: neither the Video Encoder block, which skips an encoder of rank
/// NONE, nor `webrtcsink`'s encoder discovery in WHEP Output.
///
/// The `qsv` plugin does not say which adapter a per-device feature belongs to
/// without loading it, so when any affected adapter is at risk, all QSV encoder
/// features are demoted.
///
/// Detection runs on Windows only. [`QSV_ENV`] overrides it: `on` never
/// demotes, `off` always demotes, on any platform. Call after `gst::init()`
/// and before `register_static_plugins()`: `webrtcsink` reads encoder ranks
/// when it registers.
pub fn demote_qsv_encoders() {
    let mode = qsv_mode_from_env();
    if mode == QsvMode::Off {
        let demoted = demote_all();
        tracing::info!(
            "{}=off: QSV encoders disabled ({})",
            QSV_ENV,
            if demoted.is_empty() {
                "none installed".to_string()
            } else {
                demoted.join(", ")
            }
        );
        return;
    }
    if !cfg!(windows) {
        return;
    }
    let scan = intel_adapters();
    for issue in &scan.issues {
        tracing::warn!(
            "Intel QSV check: {}. QSV encoders keep their rank. If QSV crashes Strom \
             (#1163), set {}=off",
            issue,
            QSV_ENV
        );
    }
    for adapter in &scan.intel {
        tracing::debug!(
            "Intel adapter 0x{:04X} ({}), driver {}",
            adapter.device_id,
            qsv_affected_family(adapter.device_id).unwrap_or("not affected by #1163"),
            show_version(adapter.driver)
        );
    }
    let risk = qsv_at_risk(&scan.intel);
    if mode == QsvMode::On {
        tracing::info!("{}=on: QSV encoders keep their rank", QSV_ENV);
        if let Some(risk) = risk {
            tracing::warn!(
                "{}=on overrides the #1163 check: Intel graphics driver {} on {} graphics \
                 (0x{:04X}) can corrupt the heap when QSV loads. Install {} or later",
                QSV_ENV,
                show_version(risk.driver),
                qsv_affected_family(risk.device_id).unwrap_or("Intel"),
                risk.device_id,
                show_version(Some(QSV_FIXED_INTEL_DRIVER))
            );
        }
        return;
    }
    let Some(risk) = risk else {
        let affected: Vec<String> = scan
            .intel
            .iter()
            .filter(|a| qsv_affected_family(a.device_id).is_some())
            .map(|a| format!("0x{:04X} {}", a.device_id, show_version(a.driver)))
            .collect();
        if !affected.is_empty() {
            tracing::info!(
                "Intel graphics driver has the fix for #1163: QSV encoders enabled ({})",
                affected.join(", ")
            );
        } else if !scan.intel.is_empty() {
            tracing::info!(
                "Intel graphics adapter not affected by #1163: QSV encoders keep their rank"
            );
        } else if scan.issues.is_empty() {
            tracing::info!("No Intel graphics adapter: QSV encoders keep their rank");
        }
        return;
    };
    let demoted = demote_all();
    if !demoted.is_empty() {
        tracing::warn!(
            "Intel QSV encoders disabled ({}): Intel graphics driver {} on {} graphics \
             (0x{:04X}) corrupts the heap when QSV probes its encoders (#1163). \
             Install {} or later to use them: {}. To keep them anyway, set {}=on",
            demoted.join(", "),
            show_version(risk.driver),
            qsv_affected_family(risk.device_id).unwrap_or("Intel"),
            risk.device_id,
            show_version(Some(QSV_FIXED_INTEL_DRIVER)),
            INTEL_DRIVER_DOWNLOAD,
            QSV_ENV
        );
    }
}

/// Set every QSV encoder feature's rank to NONE. Returns the names of the ones
/// that had a rank.
fn demote_all() -> Vec<String> {
    use gstreamer::prelude::*;

    qsv_encoder_features()
        .into_iter()
        .filter_map(|factory| {
            let ranked = factory.rank() != gstreamer::Rank::NONE;
            factory.set_rank(gstreamer::Rank::NONE);
            ranked.then(|| factory.name().to_string())
        })
        .collect()
}

/// The Intel display adapters, from DXGI, and what could not be read.
#[cfg(windows)]
pub fn intel_adapters() -> AdapterScan {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1};

    const INTEL: u32 = 0x8086;
    let mut scan = AdapterScan::default();
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(factory) => factory,
        Err(error) => {
            scan.issues.push(ScanIssue::Dxgi(error.to_string()));
            return scan;
        }
    };
    for index in 0.. {
        // Ends with DXGI_ERROR_NOT_FOUND after the last adapter.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let desc = match unsafe { adapter.GetDesc1() } {
            Ok(desc) => desc,
            Err(error) => {
                scan.issues.push(ScanIssue::AdapterDesc {
                    index,
                    error: error.to_string(),
                });
                continue;
            }
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
        scan.intel.push(IntelAdapter {
            device_id: desc.DeviceId,
            driver,
        });
    }
    scan
}

#[cfg(not(windows))]
pub fn intel_adapters() -> AdapterScan {
    AdapterScan::default()
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
            assert_eq!(
                qsv_affected_family(id),
                None,
                "0x{id:X} counted as affected"
            );
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

    #[test]
    fn qsv_mode_parses_the_override() {
        assert_eq!(parse_qsv_mode(None), Ok(QsvMode::Auto));
        for (value, mode) in [
            ("", QsvMode::Auto),
            ("  ", QsvMode::Auto),
            ("auto", QsvMode::Auto),
            ("AUTO", QsvMode::Auto),
            ("on", QsvMode::On),
            ("enable", QsvMode::On),
            (" On ", QsvMode::On),
            ("off", QsvMode::Off),
            ("disable", QsvMode::Off),
            ("DISABLE", QsvMode::Off),
        ] {
            assert_eq!(parse_qsv_mode(Some(value)), Ok(mode), "{value:?}");
        }
    }

    #[test]
    fn qsv_mode_rejects_other_values() {
        for value in ["1", "0", "true", "yes", "of", "enabled"] {
            assert_eq!(parse_qsv_mode(Some(value)), Err(value.to_string()));
        }
    }

    #[test]
    fn encoder_klass_matches_encoders_only() {
        assert!(is_encoder_klass("Codec/Encoder/Video/Hardware"));
        assert!(!is_encoder_klass("Codec/Decoder/Video/Hardware"));
        assert!(!is_encoder_klass("Filter/Converter/Video"));
    }

    #[test]
    fn scan_issues_name_the_failure() {
        assert_eq!(
            ScanIssue::Dxgi("0x887A0004".into()).to_string(),
            "could not open DXGI: 0x887A0004"
        );
        assert_eq!(
            ScanIssue::AdapterDesc {
                index: 1,
                error: "E_FAIL".into()
            }
            .to_string(),
            "could not read display adapter 1: E_FAIL"
        );
    }
}
