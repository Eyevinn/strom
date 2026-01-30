//! Cross-platform clipboard utilities.
//!
//! Provides clipboard functionality that works in both native and WASM environments,
//! including a fallback for insecure contexts (HTTP) where the Clipboard API is unavailable.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

// Binding for document.execCommand which isn't in web-sys
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(inline_js = r#"
export function exec_copy_command() {
    try {
        return document.execCommand('copy');
    } catch (e) {
        return false;
    }
}
"#)]
extern "C" {
    fn exec_copy_command() -> bool;
}

/// Copy text to the clipboard.
///
/// On native platforms, this uses egui's built-in clipboard support.
/// On WASM, this tries the Clipboard API first, then falls back to
/// `document.execCommand('copy')` for insecure contexts (HTTP).
///
/// Returns `true` if the copy was likely successful.
#[cfg(target_arch = "wasm32")]
pub fn copy_text(text: &str) -> bool {
    use wasm_bindgen::JsCast;

    let Some(window) = web_sys::window() else {
        tracing::error!("No window object available");
        return false;
    };

    // In secure contexts, use the modern Clipboard API
    if window.is_secure_context() {
        let clipboard = window.navigator().clipboard();
        let promise = clipboard.write_text(text);
        let future = wasm_bindgen_futures::JsFuture::from(promise);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = future.await {
                tracing::error!("Clipboard API failed: {:?}", err);
            }
        });
        return true;
    }

    // Fallback for insecure contexts: use execCommand('copy')
    tracing::debug!("Using execCommand fallback for clipboard (insecure context)");

    let Some(document) = window.document() else {
        tracing::error!("No document object available");
        return false;
    };

    // Create a temporary textarea element
    let textarea = match document.create_element("textarea") {
        Ok(el) => el,
        Err(err) => {
            tracing::error!("Failed to create textarea: {:?}", err);
            return false;
        }
    };

    let textarea: web_sys::HtmlTextAreaElement = match textarea.dyn_into() {
        Ok(el) => el,
        Err(_) => {
            tracing::error!("Failed to cast to HtmlTextAreaElement");
            return false;
        }
    };

    // Set the text and make it invisible but still selectable
    textarea.set_value(text);
    let style = textarea.style();
    let _ = style.set_property("position", "fixed");
    let _ = style.set_property("left", "-9999px");
    let _ = style.set_property("top", "0");

    // Add to document
    let body = match document.body() {
        Some(b) => b,
        None => {
            tracing::error!("No document body available");
            return false;
        }
    };

    if let Err(err) = body.append_child(&textarea) {
        tracing::error!("Failed to append textarea: {:?}", err);
        return false;
    }

    // Select the text
    textarea.select();

    // Execute copy command using our JS binding
    let result = exec_copy_command();

    // Clean up
    let _ = body.remove_child(&textarea);

    if result {
        tracing::debug!("execCommand('copy') succeeded");
    } else {
        tracing::warn!("execCommand('copy') returned false");
    }

    result
}

/// Copy text to the clipboard (native implementation).
///
/// On native platforms, this uses egui's built-in clipboard via the context.
#[cfg(not(target_arch = "wasm32"))]
pub fn copy_text_with_ctx(ctx: &egui::Context, text: &str) {
    ctx.copy_text(text.to_string());
}

/// Copy text to the clipboard using egui context.
///
/// This is the preferred method when you have access to the egui context,
/// as it provides the best cross-platform compatibility.
#[cfg(target_arch = "wasm32")]
pub fn copy_text_with_ctx(ctx: &egui::Context, text: &str) {
    // First try our custom implementation for insecure contexts
    if !web_sys::window()
        .map(|w| w.is_secure_context())
        .unwrap_or(false)
    {
        copy_text(text);
    } else {
        // In secure contexts, egui's implementation works fine
        ctx.copy_text(text.to_string());
    }
}
