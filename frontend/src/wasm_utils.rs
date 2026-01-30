//! WASM-specific utilities for JavaScript interop.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(inline_js = r#"
export function toggle_debug_console() {
    const dc = document.getElementById('debug-console');
    if (dc) {
        dc.style.display = dc.style.display === 'none' ? 'block' : 'none';
    }
}
"#)]
extern "C" {
    fn toggle_debug_console();
}

/// Toggle the debug console visibility (WASM only).
#[cfg(target_arch = "wasm32")]
pub fn toggle_debug_console_panel() {
    toggle_debug_console()
}
