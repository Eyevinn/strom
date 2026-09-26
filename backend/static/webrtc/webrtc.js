// Shared WebRTC page utilities - logging, clipboard, status, debug toggle

// --- Server log relay ---
// Batches client-side log messages and POSTs them to /api/client-log
// so server-side tooling can read browser logs without a devtools session.
const _logRelayQueue = [];
let _logRelayTimer = null;

function _flushLogRelay() {
    _logRelayTimer = null;
    if (_logRelayQueue.length === 0) return;
    const batch = _logRelayQueue.splice(0);
    fetch('/api/client-log', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(batch),
    }).catch(() => {}); // best-effort
}

function _relayLog(msg, type) {
    // Only relay logs when debug mode is enabled
    const debugCheckbox = document.getElementById('debugMode');
    if (!debugCheckbox || !debugCheckbox.checked) return;
    const level = type === 'error' ? 'error'
        : type === 'warning' ? 'warning'
        : type === 'debug' ? 'debug'
        : 'info';
    _logRelayQueue.push({ msg, level });
    if (!_logRelayTimer) {
        _logRelayTimer = setTimeout(_flushLogRelay, 500);
    }
}

/**
 * Append a timestamped entry to the log panel.
 * @param {string} msg  - message text
 * @param {string} type - CSS class: 'error', 'success', 'warning', or ''
 */
function log(msg, type) {
    const logEl = document.getElementById('log');
    if (!logEl) return;
    const entry = document.createElement('div');
    entry.className = 'log-entry ' + (type || '');
    entry.textContent = new Date().toLocaleTimeString() + ' - ' + msg;
    logEl.appendChild(entry);
    logEl.scrollTop = logEl.scrollHeight;
    _relayLog(msg, type);
}

/**
 * Copy the full log panel contents to the clipboard.
 */
function copyLog() {
    const logEl = document.getElementById('log');
    if (!logEl) return;
    const text = Array.from(logEl.children).map(e => e.textContent).join('\n');
    if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text)
            .then(() => log('Log copied', 'success'))
            .catch(() => copyFallback(text));
    } else {
        copyFallback(text);
    }
}

/**
 * Fallback clipboard copy for older browsers / non-HTTPS.
 * @param {string} text
 */
function copyFallback(text) {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.left = '-9999px';
    document.body.appendChild(ta);
    ta.select();
    try {
        document.execCommand('copy');
        log('Copied to clipboard', 'success');
    } catch (e) {
        log('Copy failed: ' + e, 'error');
    }
    document.body.removeChild(ta);
}

/**
 * Update a status bar element.
 * @param {string} elementId - DOM id of the status element
 * @param {string} message   - status text
 * @param {string} state     - CSS class: 'connected', 'connecting', 'error', 'disconnected'
 */
function setStatus(elementId, message, state) {
    const el = document.getElementById(elementId);
    if (el) {
        el.textContent = message;
        el.className = 'status ' + (state || 'disconnected');
    }
}

/**
 * Toggle a CSS class on an element by condition.
 * @param {string}  id        - DOM id
 * @param {string}  className - class to toggle
 * @param {boolean} condition - add if true, remove if false
 */
function setElementClass(id, className, condition) {
    const el = document.getElementById(id);
    if (el) {
        if (condition) {
            el.classList.add(className);
        } else {
            el.classList.remove(className);
        }
    }
}

/**
 * Toggle debug mode checkbox handler.
 * Uses a page-specific localStorage key passed via data attribute
 * on the #debugMode checkbox: <input data-storage-key="whip-debug" ...>
 */
function toggleDebugMode() {
    const checkbox = document.getElementById('debugMode');
    if (!checkbox) return;
    const enabled = checkbox.checked;
    const storageKey = checkbox.dataset.storageKey || 'webrtc-debug';
    try { localStorage.setItem(storageKey, enabled); } catch (e) {}
    if (enabled) log('Debug mode enabled', 'success');
}

/**
 * Restore debug mode from localStorage on page load.
 * @param {string} storageKey - localStorage key, e.g. 'whip-debug'
 * @returns {boolean} whether debug was enabled
 */
function restoreDebugMode(storageKey) {
    try {
        if (localStorage.getItem(storageKey) === 'true') {
            const checkbox = document.getElementById('debugMode');
            if (checkbox) checkbox.checked = true;
            return true;
        }
    } catch (e) {}
    return false;
}

/**
 * Resolve a WHIP/WHEP Location header to the session resource URL.
 * Strom's proxies return a path, which belongs to the server the offer was
 * POSTed to. That need not be the server that served the page.
 * @param {string|null} location - the Location response header
 * @param {string}      endpoint - the URL the offer was POSTed to
 * @param {string}      [pageUrl] - base for a relative endpoint
 * @returns {string|null} absolute resource URL, or null without a Location
 */
function resolveResourceUrl(location, endpoint, pageUrl = globalThis.location?.href) {
    if (!location) return null;
    return new URL(location, new URL(endpoint, pageUrl)).href;
}

// The browser loads this file as a classic script, where `module` is undefined and
// this block is skipped. Node's test runner require()s it for ../tests/.
if (typeof module !== 'undefined' && module.exports) {
    module.exports = { resolveResourceUrl };
}
