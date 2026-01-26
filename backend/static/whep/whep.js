// WHEP Connection Library

// Enable stereo for Opus codec in SDP
// Chrome defaults to mono (stereo=0) which makes stereo audio play as mono
function enableOpusStereo(sdp) {
    // Find Opus payload type from rtpmap line (e.g., "a=rtpmap:111 opus/48000/2")
    const opusMatch = sdp.match(/a=rtpmap:(\d+) opus\/48000\/2/i);
    if (!opusMatch) {
        return sdp; // No Opus codec found
    }
    const opusPayloadType = opusMatch[1];

    // Find and modify the corresponding fmtp line
    const fmtpRegex = new RegExp(`(a=fmtp:${opusPayloadType} [^\\r\\n]+)`, 'g');
    return sdp.replace(fmtpRegex, (match) => {
        if (match.includes('stereo=')) {
            return match; // Already has stereo setting
        }
        return match + ';stereo=1;sprop-stereo=1';
    });
}

class WhepConnection {
    constructor(endpoint, callbacks = {}) {
        this.endpoint = endpoint;
        this.callbacks = callbacks;
        this.peerConnection = null;
        this.resourceUrl = null;
        this.hasAudio = false;
        this.hasVideo = false;
    }

    async connect() {
        this.hasAudio = false;
        this.hasVideo = false;

        try {
            // Fetch ICE servers from the server configuration
            let iceServers = [{ urls: 'stun:stun.l.google.com:19302' }]; // fallback
            try {
                const response = await fetch('/api/ice-servers');
                if (response.ok) {
                    const config = await response.json();
                    if (config.ice_servers && config.ice_servers.length > 0) {
                        iceServers = config.ice_servers;
                    }
                }
            } catch (e) {
                console.warn('Failed to fetch ICE servers, using default:', e);
            }

            this.peerConnection = new RTCPeerConnection({ iceServers });

            this.peerConnection.ontrack = (event) => {
                if (event.track.kind === 'audio') {
                    this.hasAudio = true;
                    if (this.callbacks.onAudioTrack) {
                        this.callbacks.onAudioTrack(event.streams[0]);
                    }
                } else if (event.track.kind === 'video') {
                    this.hasVideo = true;
                    if (this.callbacks.onVideoTrack) {
                        this.callbacks.onVideoTrack(event.streams[0]);
                    }
                }
                this._updateStatus();
            };

            this.peerConnection.oniceconnectionstatechange = () => {
                const state = this.peerConnection.iceConnectionState;
                if (this.callbacks.onIceState) {
                    this.callbacks.onIceState(state);
                }
                if (state === 'connected') {
                    this._updateStatus();
                } else if (state === 'failed') {
                    if (this.callbacks.onError) {
                        this.callbacks.onError('Connection failed');
                    }
                } else if (state === 'disconnected') {
                    if (this.callbacks.onDisconnected) {
                        this.callbacks.onDisconnected();
                    }
                }
            };

            // Create both audio and video transceivers - server decides what to send
            this.peerConnection.addTransceiver('audio', { direction: 'recvonly' });
            this.peerConnection.addTransceiver('video', { direction: 'recvonly' });

            const offer = await this.peerConnection.createOffer();
            // Enable Opus stereo - Chrome defaults to mono which breaks stereo audio
            offer.sdp = enableOpusStereo(offer.sdp);
            await this.peerConnection.setLocalDescription(offer);

            if (this.callbacks.onLog) {
                this.callbacks.onLog('Created SDP offer (stereo enabled)');
            }

            // Wait for ICE gathering
            await new Promise((resolve) => {
                if (this.peerConnection.iceGatheringState === 'complete') {
                    resolve();
                } else {
                    const timeout = setTimeout(resolve, 2000);
                    this.peerConnection.onicegatheringstatechange = () => {
                        if (this.peerConnection.iceGatheringState === 'complete') {
                            clearTimeout(timeout);
                            resolve();
                        }
                    };
                }
            });

            if (this.callbacks.onLog) {
                this.callbacks.onLog('ICE gathering complete');
            }

            const response = await fetch(this.endpoint, {
                method: 'POST',
                headers: { 'Content-Type': 'application/sdp' },
                body: this.peerConnection.localDescription.sdp,
            });

            if (!response.ok) {
                const errorText = await response.text();
                throw new Error('WHEP request failed: ' + response.status + ' ' + (errorText || response.statusText));
            }

            this.resourceUrl = response.headers.get('Location');
            const answerSdp = await response.text();

            if (this.callbacks.onLog) {
                this.callbacks.onLog('Received SDP answer', 'success');
            }

            await this.peerConnection.setRemoteDescription({
                type: 'answer',
                sdp: answerSdp,
            });

            if (this.callbacks.onConnected) {
                this.callbacks.onConnected();
            }

            return true;
        } catch (error) {
            if (this.callbacks.onError) {
                this.callbacks.onError(error.message);
            }
            this.close();
            return false;
        }
    }

    _updateStatus() {
        if (this.peerConnection && this.peerConnection.iceConnectionState === 'connected') {
            if (this.callbacks.onMediaStatus) {
                this.callbacks.onMediaStatus(this.hasAudio, this.hasVideo);
            }
        }
    }

    async disconnect() {
        if (this.resourceUrl) {
            try {
                await fetch(this.resourceUrl, { method: 'DELETE' });
            } catch (e) {
                console.error('Failed to DELETE resource:', e);
            }
        }
        this.close();
        if (this.callbacks.onDisconnected) {
            this.callbacks.onDisconnected();
        }
    }

    close() {
        if (this.peerConnection) {
            this.peerConnection.close();
            this.peerConnection = null;
        }
        this.resourceUrl = null;
        this.hasAudio = false;
        this.hasVideo = false;
    }

    isConnected() {
        return this.peerConnection && this.peerConnection.iceConnectionState === 'connected';
    }
}

// UI Helper functions
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

function setStatus(elementId, message, state) {
    const el = document.getElementById(elementId);
    if (el) {
        el.textContent = message;
        el.className = 'status ' + state;
    }
}

function createAudioIndicator() {
    return `
        <div class="audio-bar"></div>
        <div class="audio-bar"></div>
        <div class="audio-bar"></div>
        <div class="audio-bar"></div>
        <div class="audio-bar"></div>
    `;
}

function escapeHtml(str) {
    const div = document.createElement('div');
    div.textContent = str;
    return div.innerHTML;
}
