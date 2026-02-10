// DeviceManager - Shared media device enumeration, selection, and hot-swap.
// Used by WHIP (input devices) and WHEP (output devices) pages.

class DeviceManager {
    /**
     * @param {Object} options
     * @param {string} options.storagePrefix - localStorage key prefix (e.g. 'whip', 'whep')
     * @param {Function} [options.onLog] - Log callback (msg, type)
     * @param {Function} [options.onDevicesChanged] - Called after re-enumeration
     */
    constructor(options = {}) {
        this.storagePrefix = options.storagePrefix || 'webrtc';
        this.onLog = options.onLog || (() => {});
        this.onDevicesChanged = options.onDevicesChanged || (() => {});

        this._videoInputs = [];
        this._audioInputs = [];
        this._audioOutputs = [];
        this._mobileCameraMode = false;
        this._cameraFlipIndex = 0;

        // Listen for device changes (plug/unplug)
        if (navigator.mediaDevices) {
            navigator.mediaDevices.addEventListener('devicechange', () => {
                this.onLog('Device change detected, re-enumerating');
                this.enumerate();
            });
        }
    }

    // ── Enumeration ──────────────────────────────────────────────

    async enumerate() {
        if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) {
            this.onLog('enumerateDevices not supported', 'warning');
            return;
        }
        try {
            const devices = await navigator.mediaDevices.enumerateDevices();
            this._videoInputs = devices.filter(d => d.kind === 'videoinput');
            this._audioInputs = devices.filter(d => d.kind === 'audioinput');
            this._audioOutputs = devices.filter(d => d.kind === 'audiooutput');
            this._mobileCameraMode = this._detectMobileCameraMode();
            this.onDevicesChanged();
        } catch (e) {
            this.onLog('Failed to enumerate devices: ' + e.message, 'error');
        }
    }

    getVideoInputs() { return this._videoInputs; }
    getAudioInputs() { return this._audioInputs; }
    getAudioOutputs() { return this._audioOutputs; }
    isMobileCameraMode() { return this._mobileCameraMode; }

    _detectMobileCameraMode() {
        if (this._videoInputs.length !== 2) return false;
        const labels = this._videoInputs.map(d => d.label.toLowerCase());
        const hasFront = labels.some(l =>
            l.includes('front') || l.includes('user') || l.includes('facing front'));
        const hasBack = labels.some(l =>
            l.includes('back') || l.includes('environment') || l.includes('rear'));
        return hasFront && hasBack;
    }

    // ── UI Population ────────────────────────────────────────────

    /**
     * Populate camera UI: either a dropdown or a flip button.
     * @param {HTMLSelectElement} selectEl - Camera dropdown
     * @param {HTMLButtonElement} flipBtnEl - Camera flip button
     */
    populateCameraUI(selectEl, flipBtnEl) {
        if (this._mobileCameraMode) {
            selectEl.style.display = 'none';
            flipBtnEl.style.display = '';
        } else {
            selectEl.style.display = '';
            flipBtnEl.style.display = 'none';
            this._populateSelect(selectEl, this._videoInputs, 'Camera');
            this._restoreSelection(selectEl, 'camera');
        }
    }

    /**
     * Populate microphone dropdown.
     * @param {HTMLSelectElement} selectEl
     */
    populateMicSelect(selectEl) {
        this._populateSelect(selectEl, this._audioInputs, 'Microphone');
        this._restoreSelection(selectEl, 'mic');
    }

    /**
     * Populate audio output dropdown.
     * @param {HTMLSelectElement} selectEl
     * @returns {boolean} true if dropdown should be shown (2+ devices and setSinkId supported)
     */
    populateAudioOutputSelect(selectEl) {
        // Feature-detect setSinkId
        const testEl = document.createElement('audio');
        if (typeof testEl.setSinkId !== 'function') {
            return false;
        }
        if (this._audioOutputs.length <= 1) {
            return false;
        }
        this._populateSelect(selectEl, this._audioOutputs, 'Speaker');
        this._restoreSelection(selectEl, 'audioOutput');
        return true;
    }

    _populateSelect(selectEl, devices, fallbackPrefix) {
        const previousValue = selectEl.value;
        selectEl.innerHTML = '';
        if (devices.length === 0) {
            const opt = document.createElement('option');
            opt.value = '';
            opt.textContent = 'No ' + fallbackPrefix.toLowerCase() + 's found';
            selectEl.appendChild(opt);
            return;
        }
        devices.forEach((device, i) => {
            const opt = document.createElement('option');
            opt.value = device.deviceId;
            opt.textContent = device.label || (fallbackPrefix + ' ' + (i + 1));
            selectEl.appendChild(opt);
        });
        // Try to keep previous selection
        if (previousValue) {
            const exists = Array.from(selectEl.options).some(o => o.value === previousValue);
            if (exists) selectEl.value = previousValue;
        }
    }

    // ── Hot-swap: Camera ─────────────────────────────────────────

    /**
     * Switch camera by device ID. Works during active call (replaceTrack).
     * @param {string} deviceId
     * @param {Object} extraConstraints - e.g. { height: { ideal: 720 }, width: { ideal: 1280 } }
     * @param {RTCPeerConnection|null} peerConnection - If connected, replaceTrack on video sender
     * @returns {{ newTrack: MediaStreamTrack, oldTrack: MediaStreamTrack|null }} or null on failure
     */
    async switchCamera(deviceId, extraConstraints, peerConnection) {
        if (!deviceId) return null;
        const videoConstraints = Object.assign({ deviceId: { exact: deviceId } }, extraConstraints || {});

        try {
            const newStream = await navigator.mediaDevices.getUserMedia({ video: videoConstraints });
            const newTrack = newStream.getVideoTracks()[0];
            // Stop any audio tracks from the new stream (we only wanted video)
            newStream.getAudioTracks().forEach(t => t.stop());

            let oldTrack = null;
            if (peerConnection) {
                const sender = peerConnection.getSenders().find(s => s.track && s.track.kind === 'video');
                if (sender) {
                    oldTrack = sender.track;
                    await sender.replaceTrack(newTrack);
                }
            }

            this.saveSelection('camera', deviceId);
            return { newTrack, oldTrack };
        } catch (e) {
            this.onLog('Failed to switch camera: ' + e.message, 'error');
            return null;
        }
    }

    /**
     * Flip camera (mobile mode). Cycles through video inputs.
     * @param {Object} extraConstraints
     * @param {RTCPeerConnection|null} peerConnection
     * @returns {{ newTrack, oldTrack, deviceIndex }} or null
     */
    async flipCamera(extraConstraints, peerConnection) {
        if (this._videoInputs.length === 0) return null;
        this._cameraFlipIndex = (this._cameraFlipIndex + 1) % this._videoInputs.length;
        const device = this._videoInputs[this._cameraFlipIndex];
        const result = await this.switchCamera(device.deviceId, extraConstraints, peerConnection);
        if (result) {
            result.deviceIndex = this._cameraFlipIndex;
            this._saveItem('cameraFlipIndex', this._cameraFlipIndex);
        }
        return result;
    }

    /**
     * Get the current flip index (for label display).
     */
    getCameraFlipIndex() {
        return this._cameraFlipIndex;
    }

    /**
     * Get the label of the current flip camera.
     */
    getCameraFlipLabel() {
        const device = this._videoInputs[this._cameraFlipIndex];
        if (!device) return '';
        const label = device.label.toLowerCase();
        if (label.includes('front') || label.includes('user')) return 'Front';
        if (label.includes('back') || label.includes('rear') || label.includes('environment')) return 'Back';
        return device.label || ('Camera ' + (this._cameraFlipIndex + 1));
    }

    // ── Hot-swap: Microphone ─────────────────────────────────────

    /**
     * Switch microphone by device ID. Works during active call (replaceTrack).
     * @param {string} deviceId
     * @param {Object} extraConstraints - e.g. { echoCancellation: true }
     * @param {RTCPeerConnection|null} peerConnection
     * @returns {{ newTrack, oldTrack }} or null
     */
    async switchMic(deviceId, extraConstraints, peerConnection) {
        if (!deviceId) return null;
        const audioConstraints = Object.assign({
            deviceId: { exact: deviceId },
            echoCancellation: true,
            noiseSuppression: true,
            autoGainControl: true,
        }, extraConstraints || {});

        try {
            const newStream = await navigator.mediaDevices.getUserMedia({ audio: audioConstraints });
            const newTrack = newStream.getAudioTracks()[0];
            newStream.getVideoTracks().forEach(t => t.stop());

            let oldTrack = null;
            if (peerConnection) {
                const sender = peerConnection.getSenders().find(s => s.track && s.track.kind === 'audio');
                if (sender) {
                    oldTrack = sender.track;
                    await sender.replaceTrack(newTrack);
                }
            }

            this.saveSelection('mic', deviceId);
            return { newTrack, oldTrack };
        } catch (e) {
            this.onLog('Failed to switch microphone: ' + e.message, 'error');
            return null;
        }
    }

    // ── Audio Output ─────────────────────────────────────────────

    /**
     * Set audio output device on one or more media elements.
     * @param {string} deviceId
     * @param {...HTMLMediaElement} elements - audio/video elements
     * @returns {boolean} success
     */
    async setAudioOutput(deviceId, ...elements) {
        try {
            for (const el of elements) {
                if (typeof el.setSinkId === 'function') {
                    await el.setSinkId(deviceId);
                }
            }
            this.saveSelection('audioOutput', deviceId);
            return true;
        } catch (e) {
            this.onLog('Failed to set audio output: ' + e.message, 'error');
            return false;
        }
    }

    // ── Persistence ──────────────────────────────────────────────

    saveSelection(kind, value) {
        this._saveItem(kind, value);
    }

    getSavedSelection(kind) {
        return this._getItem(kind);
    }

    _saveItem(key, value) {
        try {
            localStorage.setItem(this.storagePrefix + '-device-' + key, value);
        } catch (e) { /* quota exceeded or private mode */ }
    }

    _getItem(key) {
        try {
            return localStorage.getItem(this.storagePrefix + '-device-' + key);
        } catch (e) { return null; }
    }

    _restoreSelection(selectEl, kind) {
        const saved = this.getSavedSelection(kind);
        if (saved) {
            const exists = Array.from(selectEl.options).some(o => o.value === saved);
            if (exists) selectEl.value = saved;
        }
    }

    /**
     * Restore flip index from localStorage.
     */
    restoreCameraFlipIndex() {
        const saved = this._getItem('cameraFlipIndex');
        if (saved !== null) {
            const idx = parseInt(saved);
            if (!isNaN(idx) && idx >= 0 && idx < this._videoInputs.length) {
                this._cameraFlipIndex = idx;
            }
        }
    }
}
