// WHEP Connection Library with extensive TURN/ICE debugging

// Global debug mode flag - toggle via UI or setWhepDebugMode(true/false)
let whepDebugMode = false;

function setWhepDebugMode(enabled) {
    whepDebugMode = enabled;
    console.log('[WHEP] Debug mode ' + (enabled ? 'enabled' : 'disabled'));
}

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

// Strip Opus stereo/sprop-stereo fmtp params from SDP before sending to server.
// webrtcsink uses these fmtp values as capsfilter constraints in its codec
// discovery pipeline. rtpopuspay cannot produce stereo= in its output caps,
// so the discovery capsfilter blocks and the 30s timeout fires.
// The local description keeps stereo=1 so Chrome decodes stereo correctly.
function stripOpusStereoForServer(sdp) {
    return sdp.replace(/;stereo=1/g, '').replace(/;sprop-stereo=1/g, '');
}

// Extract ICE candidates from SDP for debugging
function extractIceCandidatesFromSdp(sdp) {
    const candidates = [];
    const lines = sdp.split('\n');
    for (const line of lines) {
        if (line.startsWith('a=candidate:')) {
            candidates.push(line.trim());
        }
    }
    return candidates;
}

// Parse ICE candidate for detailed logging
function parseIceCandidate(candidateStr) {
    // Format: candidate:foundation component protocol priority ip port typ type [raddr addr] [rport port]
    const parts = candidateStr.split(' ');
    if (parts.length < 8) return { raw: candidateStr };

    const result = {
        foundation: parts[0].replace('candidate:', ''),
        component: parts[1],
        protocol: parts[2],
        priority: parts[3],
        ip: parts[4],
        port: parts[5],
        type: parts[7], // host, srflx, prflx, relay
        raw: candidateStr
    };

    // Extract raddr/rport for relay/srflx candidates
    for (let i = 8; i < parts.length - 1; i++) {
        if (parts[i] === 'raddr') result.relatedAddress = parts[i + 1];
        if (parts[i] === 'rport') result.relatedPort = parts[i + 1];
    }

    return result;
}

class WhepConnection {
    constructor(endpoint, callbacks = {}) {
        this.endpoint = endpoint;
        this.callbacks = callbacks;
        this.peerConnection = null;
        this.resourceUrl = null;
        this.hasAudio = false;
        this.hasVideo = false;
        this.localCandidates = [];
        this.remoteCandidates = [];
        this.iceConfig = null;
        this.statsInterval = null;
    }

    // Always log (for essential messages like errors, connection status)
    _logAlways(message, type = '') {
        const timestamp = new Date().toISOString();
        console.log(`[WHEP ${timestamp}] ${message}`);
        if (this.callbacks.onLog) {
            this.callbacks.onLog(message, type);
        }
    }

    // Only log when debug mode is enabled
    _log(message, type = '') {
        if (!whepDebugMode) return;
        this._logAlways(message, type);
    }

    _logDebug(message) {
        if (!whepDebugMode) return;
        const timestamp = new Date().toISOString();
        console.log(`[WHEP DEBUG ${timestamp}] ${message}`);
        if (this.callbacks.onLog) {
            this.callbacks.onLog(`[DEBUG] ${message}`, 'debug');
        }
    }

    async connect() {
        this.hasAudio = false;
        this.hasVideo = false;
        this.localCandidates = [];
        this.remoteCandidates = [];

        try {
            // Fetch ICE servers and transport policy from the server configuration
            let iceServers = [{ urls: 'stun:stun.l.google.com:19302' }]; // fallback
            let iceTransportPolicy = 'all'; // fallback

            this._log('Fetching ICE server configuration from /api/ice-servers...');

            try {
                const response = await fetch('/api/ice-servers');
                if (response.ok) {
                    const config = await response.json();
                    this._logDebug('ICE config response: ' + JSON.stringify(config, null, 2));
                    if (config.ice_servers && config.ice_servers.length > 0) {
                        iceServers = config.ice_servers;
                    }
                    if (config.ice_transport_policy) {
                        iceTransportPolicy = config.ice_transport_policy;
                    }
                } else {
                    this._log('Failed to fetch ICE servers: HTTP ' + response.status, 'warning');
                }
            } catch (e) {
                this._log('Failed to fetch ICE servers: ' + e.message, 'warning');
            }

            // Log the ICE configuration being used
            this.iceConfig = { iceServers, iceTransportPolicy };
            this._log('=== ICE CONFIGURATION ===');
            this._log('iceTransportPolicy: ' + iceTransportPolicy);
            this._log('iceServers:');
            for (const server of iceServers) {
                this._log('  - urls: ' + server.urls);
                if (server.username) this._log('    username: ' + server.username);
                if (server.credential) this._log('    credential: ***');
            }
            this._log('=========================');

            // Create RTCPeerConnection with configured transport policy
            this._log('Creating RTCPeerConnection with iceTransportPolicy=' + iceTransportPolicy);
            this.peerConnection = new RTCPeerConnection({ iceServers, iceTransportPolicy });

            // Log all state changes
            this.peerConnection.onconnectionstatechange = () => {
                this._log('Connection state: ' + this.peerConnection.connectionState,
                    this.peerConnection.connectionState === 'connected' ? 'success' : '');
            };

            this.peerConnection.onsignalingstatechange = () => {
                this._logDebug('Signaling state: ' + this.peerConnection.signalingState);
            };

            this.peerConnection.onicegatheringstatechange = () => {
                this._log('ICE gathering state: ' + this.peerConnection.iceGatheringState);
            };

            // Track ICE candidates
            this.peerConnection.onicecandidate = (event) => {
                if (event.candidate) {
                    const parsed = parseIceCandidate(event.candidate.candidate);
                    this.localCandidates.push(parsed);
                    this._log('Local ICE candidate: type=' + parsed.type +
                        ' protocol=' + parsed.protocol +
                        ' ip=' + parsed.ip + ':' + parsed.port +
                        (parsed.relatedAddress ? ' relay-from=' + parsed.relatedAddress + ':' + parsed.relatedPort : ''));
                    this._logDebug('Full candidate: ' + event.candidate.candidate);
                } else {
                    this._log('ICE candidate gathering complete (null candidate)');
                    this._logLocalCandidateSummary();
                }
            };

            this.peerConnection.onicecandidateerror = (event) => {
                this._logAlways('ICE candidate error: ' + event.errorText +
                    ' (code=' + event.errorCode +
                    ' url=' + event.url +
                    ' host=' + event.address + ':' + event.port + ')', 'error');
            };

            this.peerConnection.ontrack = (event) => {
                this._log('Track received: kind=' + event.track.kind + ' id=' + event.track.id);
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
                this._log('ICE connection state: ' + state,
                    state === 'connected' ? 'success' :
                    state === 'failed' ? 'error' : '');

                if (this.callbacks.onIceState) {
                    this.callbacks.onIceState(state);
                }

                if (state === 'connected' || state === 'completed') {
                    this._updateStatus();
                    this._logConnectionStats();
                    // Start periodic stats logging
                    this._startStatsLogging();
                } else if (state === 'failed') {
                    this._logAlways('ICE connection failed', 'error');
                    this._logDebugSummary();
                    if (this.callbacks.onError) {
                        this.callbacks.onError('ICE connection failed - check TURN server');
                    }
                } else if (state === 'disconnected') {
                    this._logAlways('ICE disconnected', 'warning');
                    if (this.callbacks.onDisconnected) {
                        this.callbacks.onDisconnected();
                    }
                }
            };

            // Create both audio and video transceivers - server decides what to send
            this._log('Adding transceivers (audio + video, recvonly)');
            this.peerConnection.addTransceiver('audio', { direction: 'recvonly' });
            this.peerConnection.addTransceiver('video', { direction: 'recvonly' });

            const offer = await this.peerConnection.createOffer();
            // Enable Opus stereo - Chrome defaults to mono which breaks stereo audio
            offer.sdp = enableOpusStereo(offer.sdp);
            await this.peerConnection.setLocalDescription(offer);

            this._log('Created SDP offer');
            this._logDebug('=== LOCAL SDP OFFER ===\n' + offer.sdp);

            // Wait for ICE gathering with detailed logging
            this._log('Waiting for ICE gathering (timeout: 5s)...');
            const gatheringStartTime = Date.now();

            await new Promise((resolve) => {
                if (this.peerConnection.iceGatheringState === 'complete') {
                    resolve();
                } else {
                    const timeout = setTimeout(() => {
                        this._log('ICE gathering timeout after 5s', 'warning');
                        resolve();
                    }, 5000);
                    this.peerConnection.onicegatheringstatechange = () => {
                        if (this.peerConnection.iceGatheringState === 'complete') {
                            clearTimeout(timeout);
                            resolve();
                        }
                    };
                }
            });

            const gatheringTime = Date.now() - gatheringStartTime;
            this._log('ICE gathering completed in ' + gatheringTime + 'ms');

            // Log candidates in final SDP
            const localSdp = this.peerConnection.localDescription.sdp;
            const sdpCandidates = extractIceCandidatesFromSdp(localSdp);
            this._log('Candidates in SDP offer: ' + sdpCandidates.length);
            for (const c of sdpCandidates) {
                this._logDebug('SDP candidate: ' + c);
            }

            // Send offer to WHEP endpoint
            // Strip stereo fmtp params that break webrtcsink's codec discovery
            const serverSdp = stripOpusStereoForServer(localSdp);
            this._log('Sending SDP offer to ' + this.endpoint);
            const response = await fetch(this.endpoint, {
                method: 'POST',
                headers: { 'Content-Type': 'application/sdp' },
                body: serverSdp,
            });

            if (!response.ok) {
                const errorText = await response.text();
                throw new Error('WHEP request failed: ' + response.status + ' ' + (errorText || response.statusText));
            }

            this.resourceUrl = response.headers.get('Location');
            const answerSdp = await response.text();

            this._log('Received SDP answer', 'success');
            this._logDebug('=== REMOTE SDP ANSWER ===\n' + answerSdp);

            // Extract and log remote candidates from answer
            const remoteSdpCandidates = extractIceCandidatesFromSdp(answerSdp);
            this._log('Candidates in SDP answer: ' + remoteSdpCandidates.length);
            for (const c of remoteSdpCandidates) {
                const parsed = parseIceCandidate(c.replace('a=', ''));
                this.remoteCandidates.push(parsed);
                this._log('Remote ICE candidate: type=' + parsed.type +
                    ' protocol=' + parsed.protocol +
                    ' ip=' + parsed.ip + ':' + parsed.port);
            }

            await this.peerConnection.setRemoteDescription({
                type: 'answer',
                sdp: answerSdp,
            });

            this._log('Remote description set, waiting for ICE to connect...');

            if (this.callbacks.onConnected) {
                this.callbacks.onConnected();
            }

            return true;
        } catch (error) {
            this._logAlways('Connection error: ' + error.message, 'error');
            this._logDebugSummary();
            if (this.callbacks.onError) {
                this.callbacks.onError(error.message);
            }
            this.close();
            return false;
        }
    }

    _logLocalCandidateSummary() {
        this._log('=== LOCAL CANDIDATE SUMMARY ===');
        const byType = {};
        for (const c of this.localCandidates) {
            byType[c.type] = (byType[c.type] || 0) + 1;
        }
        for (const [type, count] of Object.entries(byType)) {
            this._log('  ' + type + ': ' + count);
        }
        if (this.localCandidates.length === 0) {
            this._log('  (no candidates gathered - TURN server may be unreachable)', 'warning');
        }
        this._log('================================');
    }

    _logDebugSummary() {
        this._log('=== DEBUG SUMMARY ===');
        this._log('ICE config: ' + JSON.stringify(this.iceConfig));
        this._log('Local candidates: ' + this.localCandidates.length);
        this._log('Remote candidates: ' + this.remoteCandidates.length);
        if (this.peerConnection) {
            this._log('ICE connection state: ' + this.peerConnection.iceConnectionState);
            this._log('ICE gathering state: ' + this.peerConnection.iceGatheringState);
            this._log('Connection state: ' + this.peerConnection.connectionState);
            this._log('Signaling state: ' + this.peerConnection.signalingState);
        }
        this._log('=====================');
    }

    async _logConnectionStats() {
        if (!this.peerConnection) return;

        try {
            const stats = await this.peerConnection.getStats();

            this._log('=== CONNECTION STATS ===');

            stats.forEach(report => {
                if (report.type === 'candidate-pair' && report.state === 'succeeded') {
                    this._log('Active candidate pair:');
                    this._log('  local: ' + report.localCandidateId);
                    this._log('  remote: ' + report.remoteCandidateId);
                    this._log('  state: ' + report.state);
                    this._log('  nominated: ' + report.nominated);
                    if (report.currentRoundTripTime) {
                        this._log('  RTT: ' + (report.currentRoundTripTime * 1000).toFixed(1) + 'ms');
                    }
                    if (report.availableOutgoingBitrate) {
                        this._log('  available bitrate: ' + (report.availableOutgoingBitrate / 1000).toFixed(0) + ' kbps');
                    }
                }

                if (report.type === 'local-candidate' || report.type === 'remote-candidate') {
                    // Find if this candidate is part of the active pair
                    const prefix = report.type === 'local-candidate' ? 'Local' : 'Remote';
                    this._logDebug(prefix + ' candidate [' + report.id + ']: ' +
                        'type=' + report.candidateType +
                        ' protocol=' + report.protocol +
                        ' address=' + report.address + ':' + report.port +
                        (report.relayProtocol ? ' relayProtocol=' + report.relayProtocol : ''));
                }

                if (report.type === 'transport') {
                    this._log('Transport:');
                    this._log('  dtlsState: ' + report.dtlsState);
                    this._log('  iceState: ' + report.iceState);
                    this._log('  selectedCandidatePairId: ' + report.selectedCandidatePairId);
                    if (report.selectedCandidatePairChanges) {
                        this._log('  pair changes: ' + report.selectedCandidatePairChanges);
                    }
                }
            });

            this._log('========================');
        } catch (e) {
            this._logDebug('Failed to get stats: ' + e.message);
        }
    }

    _startStatsLogging() {
        // Only start stats logging if debug mode is enabled
        if (!whepDebugMode) return;

        // Log stats every 5 seconds while connected
        if (this.statsInterval) clearInterval(this.statsInterval);

        this.statsInterval = setInterval(async () => {
            // Stop if debug mode disabled, disconnected, or connection gone
            if (!whepDebugMode || !this.peerConnection || this.peerConnection.iceConnectionState !== 'connected') {
                clearInterval(this.statsInterval);
                this.statsInterval = null;
                return;
            }

            try {
                const stats = await this.peerConnection.getStats();
                let bytesReceived = 0;
                let packetsLost = 0;
                let packetsReceived = 0;
                let rtt = null;

                stats.forEach(report => {
                    if (report.type === 'inbound-rtp') {
                        bytesReceived += report.bytesReceived || 0;
                        packetsLost += report.packetsLost || 0;
                        packetsReceived += report.packetsReceived || 0;
                    }
                    if (report.type === 'candidate-pair' && report.state === 'succeeded') {
                        rtt = report.currentRoundTripTime;
                    }
                });

                const lossRate = packetsReceived > 0 ?
                    ((packetsLost / (packetsLost + packetsReceived)) * 100).toFixed(2) : 0;

                this._logDebug('Stats: received=' + (bytesReceived / 1024).toFixed(0) + 'KB' +
                    ' packets=' + packetsReceived +
                    ' lost=' + packetsLost + ' (' + lossRate + '%)' +
                    (rtt ? ' rtt=' + (rtt * 1000).toFixed(1) + 'ms' : ''));
            } catch (e) {
                // Ignore stats errors
            }
        }, 5000);
    }

    _updateStatus() {
        if (this.peerConnection && this.peerConnection.iceConnectionState === 'connected') {
            if (this.callbacks.onMediaStatus) {
                this.callbacks.onMediaStatus(this.hasAudio, this.hasVideo);
            }
        }
    }

    async disconnect() {
        if (this.statsInterval) {
            clearInterval(this.statsInterval);
            this.statsInterval = null;
        }

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
        if (this.statsInterval) {
            clearInterval(this.statsInterval);
            this.statsInterval = null;
        }

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
