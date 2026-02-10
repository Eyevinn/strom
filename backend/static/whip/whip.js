// WHIP Client - WebRTC-HTTP Ingestion Protocol client for browser-based sending.

// Global debug mode flag - toggle via UI or setWhipDebugMode(true/false)
let whipDebugMode = false;

function setWhipDebugMode(enabled) {
    whipDebugMode = enabled;
    console.log('[WHIP] Debug mode ' + (enabled ? 'enabled' : 'disabled'));
}

// Enable stereo for Opus codec in SDP
// Chrome defaults to mono (stereo=0) which makes stereo audio play as mono
function enableOpusStereo(sdp) {
    // Find Opus payload type from rtpmap line (e.g., "a=rtpmap:111 opus/48000/2")
    const opusMatch = sdp.match(/a=rtpmap:(\d+) opus\/48000\/2/i);
    if (!opusMatch) return sdp;
    const opusPt = opusMatch[1];
    const fmtpRegex = new RegExp(`(a=fmtp:${opusPt} [^\\r\\n]+)`, 'g');
    return sdp.replace(fmtpRegex, (match) => {
        if (match.includes('stereo=')) return match;
        return match + ';stereo=1;sprop-stereo=1';
    });
}

// Parse ICE candidate for detailed logging
function parseIceCandidate(candidateStr) {
    const parts = candidateStr.split(' ');
    if (parts.length < 8) return { raw: candidateStr };
    const result = {
        foundation: parts[0].replace('candidate:', ''),
        component: parts[1],
        protocol: parts[2],
        priority: parts[3],
        ip: parts[4],
        port: parts[5],
        type: parts[7],
        raw: candidateStr
    };
    for (let i = 8; i < parts.length - 1; i++) {
        if (parts[i] === 'raddr') result.relatedAddress = parts[i + 1];
        if (parts[i] === 'rport') result.relatedPort = parts[i + 1];
    }
    return result;
}

class WhipClient {
    /**
     * @param {string} endpoint - The WHIP endpoint URL (e.g., /whip/my-stream)
     * @param {Object} callbacks - Event callbacks
     */
    constructor(endpoint, callbacks = {}) {
        this.endpoint = endpoint;
        this.callbacks = callbacks;
        this.peerConnection = null;
        this.resourceUrl = null;
        this.localStream = null;
        this.connected = false;
        this.localCandidates = [];
    }

    // Always log (errors, connection status)
    _logAlways(msg, type = '') {
        const timestamp = new Date().toISOString();
        console.log(`[WHIP ${timestamp}] ${msg}`);
        if (this.callbacks.onLog) {
            this.callbacks.onLog(msg, type);
        }
    }

    // Only log when debug mode is enabled
    _log(msg, type = '') {
        if (!whipDebugMode) return;
        this._logAlways(msg, type);
    }

    _logDebug(msg) {
        if (!whipDebugMode) return;
        const timestamp = new Date().toISOString();
        console.log(`[WHIP DEBUG ${timestamp}] ${msg}`);
        if (this.callbacks.onLog) {
            this.callbacks.onLog(`[DEBUG] ${msg}`, 'debug');
        }
    }

    /**
     * Get user media (camera/microphone).
     * @param {Object} constraints - getUserMedia constraints
     * @returns {MediaStream}
     */
    async getMedia(constraints) {
        this._logAlways('Requesting user media...');
        if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
            const msg = window.isSecureContext
                ? 'getUserMedia not supported by this browser'
                : 'Camera/mic requires HTTPS. Connect via localhost or enable HTTPS.';
            this._logAlways(msg, 'error');
            throw new Error(msg);
        }
        try {
            this.localStream = await navigator.mediaDevices.getUserMedia(constraints);
            const tracks = this.localStream.getTracks().map(t => t.kind + ':' + t.label);
            this._logAlways('Got media tracks: ' + tracks.join(', '));
            return this.localStream;
        } catch (e) {
            this._logAlways('Failed to get user media: ' + e.message, 'error');
            throw e;
        }
    }

    /**
     * Connect to the WHIP endpoint and start sending media.
     * @param {MediaStream} stream - The media stream to send
     */
    async connect(stream) {
        if (this.connected) {
            this._logAlways('Already connected, disconnect first', 'error');
            return;
        }

        this.localCandidates = [];

        try {
            // Fetch ICE servers and transport policy from the server configuration
            let iceServers = [];
            let iceTransportPolicy = 'all';

            this._log('Fetching ICE server configuration from /api/ice-servers...');

            try {
                const resp = await fetch('/api/ice-servers');
                if (resp.ok) {
                    const config = await resp.json();
                    this._logDebug('ICE config response: ' + JSON.stringify(config, null, 2));
                    if (config.ice_servers && config.ice_servers.length > 0) {
                        iceServers = config.ice_servers;
                    }
                    if (config.ice_transport_policy) {
                        iceTransportPolicy = config.ice_transport_policy;
                    }
                } else {
                    this._log('Failed to fetch ICE servers: HTTP ' + resp.status, 'warning');
                }
            } catch (e) {
                this._log('Failed to fetch ICE servers: ' + e.message, 'warning');
            }

            // Log ICE configuration
            this._log('=== ICE CONFIGURATION ===');
            this._log('iceTransportPolicy: ' + iceTransportPolicy);
            this._log('iceServers:');
            for (const server of iceServers) {
                this._log('  - urls: ' + server.urls);
                if (server.username) this._log('    username: ' + server.username);
                if (server.credential) this._log('    credential: ***');
            }
            this._log('=========================');

            // Create RTCPeerConnection
            this._log('Creating RTCPeerConnection with iceTransportPolicy=' + iceTransportPolicy);
            this.peerConnection = new RTCPeerConnection({
                iceServers: iceServers.length > 0 ? iceServers : undefined,
                iceTransportPolicy,
                bundlePolicy: 'max-bundle',
            });

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

            this.peerConnection.oniceconnectionstatechange = () => {
                if (!this.peerConnection) return;
                const state = this.peerConnection.iceConnectionState;
                this._log('ICE connection state: ' + state,
                    state === 'connected' ? 'success' :
                    state === 'failed' ? 'error' : '');
                if (this.callbacks.onIceState) {
                    this.callbacks.onIceState(state);
                }
                if (state === 'connected' || state === 'completed') {
                    this.connected = true;
                    this._logConnectionStats();
                    if (this.callbacks.onConnected) {
                        this.callbacks.onConnected();
                    }
                } else if (state === 'failed') {
                    this._logAlways('ICE connection failed', 'error');
                    this._logDebugSummary();
                    this.connected = false;
                    if (this.callbacks.onDisconnected) {
                        this.callbacks.onDisconnected();
                    }
                } else if (state === 'disconnected') {
                    this._logAlways('ICE disconnected', 'warning');
                    this.connected = false;
                    if (this.callbacks.onDisconnected) {
                        this.callbacks.onDisconnected();
                    }
                }
            };

            this.peerConnection.onconnectionstatechange = () => {
                if (!this.peerConnection) return;
                const state = this.peerConnection.connectionState;
                this._log('Connection state: ' + state,
                    state === 'connected' ? 'success' : '');
                if (state === 'failed' && this.connected) {
                    this.connected = false;
                    if (this.callbacks.onDisconnected) {
                        this.callbacks.onDisconnected();
                    }
                }
            };

            this.peerConnection.onsignalingstatechange = () => {
                this._logDebug('Signaling state: ' + this.peerConnection.signalingState);
            };

            this.peerConnection.onicegatheringstatechange = () => {
                this._log('ICE gathering state: ' + this.peerConnection.iceGatheringState);
            };

            // Add tracks from the stream
            for (const track of stream.getTracks()) {
                this._log('Adding ' + track.kind + ' track: ' + track.label);
                const transceiver = this.peerConnection.addTransceiver(track, { direction: 'sendonly' });

                // Set encoding parameters for video
                if (track.kind === 'video') {
                    try {
                        const params = transceiver.sender.getParameters();
                        if (!params.encodings || params.encodings.length === 0) {
                            params.encodings = [{}];
                        }
                        params.encodings[0].maxBitrate = 4_000_000; // 4 Mbps
                        params.encodings[0].maxFramerate = 30;
                        await transceiver.sender.setParameters(params);
                        this._log('Set video encoding: maxBitrate=4Mbps, maxFramerate=30');
                    } catch (e) {
                        this._logAlways('Failed to set video encoding params: ' + e.message, 'error');
                    }
                }
            }

            // Create SDP offer
            this._log('Creating SDP offer...');
            const offer = await this.peerConnection.createOffer();

            // Enable stereo for Opus if present
            offer.sdp = enableOpusStereo(offer.sdp);

            // Log SDP offer details
            this._log('SDP offer key lines:');
            for (const line of offer.sdp.split('\r\n')) {
                if (line.startsWith('m=video') || line.startsWith('b=AS') ||
                    line.startsWith('a=rtpmap:') || line.startsWith('a=fmtp:') ||
                    line.startsWith('a=extmap:') ||
                    line.includes('transport-cc') || line.includes('remb') ||
                    line.includes('x-google')) {
                    this._log('  ' + line);
                }
            }

            await this.peerConnection.setLocalDescription(offer);

            // Wait for ICE gathering to complete (or timeout)
            this._log('Waiting for ICE gathering (timeout: 5s)...');
            const gatheringStartTime = Date.now();

            await new Promise((resolve) => {
                if (this.peerConnection.iceGatheringState === 'complete') {
                    resolve();
                    return;
                }
                const timeout = setTimeout(() => {
                    this._log('ICE gathering timeout after 5s', 'warning');
                    resolve();
                }, 5000);
                this.peerConnection.onicegatheringstatechange = () => {
                    if (this.peerConnection.iceGatheringState === 'complete') {
                        clearTimeout(timeout);
                        this._log('ICE gathering complete');
                        resolve();
                    }
                };
            });

            const gatheringTime = Date.now() - gatheringStartTime;
            this._log('ICE gathering completed in ' + gatheringTime + 'ms');

            const finalOffer = this.peerConnection.localDescription;
            this._log('Sending SDP offer to ' + this.endpoint);
            this._logDebug('=== LOCAL SDP OFFER ===\n' + finalOffer.sdp);

            // POST the offer to the WHIP endpoint.
            // Retry on 500 errors - whipserversrc may need time to clean up after
            // a previous session before it can accept a new one.
            const maxRetries = 3;
            const retryDelayMs = 2000;
            let response;
            for (let attempt = 1; attempt <= maxRetries; attempt++) {
                try {
                    response = await fetch(this.endpoint, {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/sdp' },
                        body: finalOffer.sdp,
                    });
                } catch (e) {
                    throw new Error('Failed to send offer: ' + e.message);
                }

                if (response.ok) break;

                if (response.status >= 500 && attempt < maxRetries) {
                    this._logAlways('Server returned ' + response.status + ', retrying in ' + (retryDelayMs / 1000) + 's (attempt ' + attempt + '/' + maxRetries + ')...', 'warning');
                    await new Promise(r => setTimeout(r, retryDelayMs));
                    continue;
                }

                const errorText = await response.text();
                throw new Error('WHIP server returned ' + response.status + ': ' + errorText);
            }

            // Get the resource URL for cleanup
            const locationHeader = response.headers.get('Location');
            if (locationHeader) {
                this.resourceUrl = locationHeader.startsWith('/')
                    ? window.location.origin + locationHeader
                    : locationHeader;
                this._log('Resource URL: ' + this.resourceUrl);
            }

            // Set the SDP answer
            const answerSdp = await response.text();
            this._log('Received SDP answer (' + answerSdp.length + ' bytes)', 'success');
            this._logDebug('=== REMOTE SDP ANSWER ===\n' + answerSdp);

            // Log key SDP answer lines
            this._log('SDP answer key lines:');
            for (const line of answerSdp.split('\n')) {
                const l = line.trim();
                if (l.startsWith('m=video') || l.startsWith('b=') ||
                    l.startsWith('a=rtpmap:') || l.startsWith('a=fmtp:') ||
                    l.includes('transport-cc') || l.includes('remb') ||
                    l.includes('rtcp-fb') || l.startsWith('a=extmap:')) {
                    this._log('  ' + l);
                }
            }

            await this.peerConnection.setRemoteDescription({
                type: 'answer',
                sdp: answerSdp,
            });

            this._log('Remote description set, waiting for ICE to connect...');

        } catch (error) {
            this._logAlways('Connection error: ' + error.message, 'error');
            this._logDebugSummary();
            if (this.callbacks.onError) {
                this.callbacks.onError(error.message);
            }
            this.cleanup();
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
        this._log('Local candidates: ' + this.localCandidates.length);
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
                    if (report.currentRoundTripTime) {
                        this._log('  RTT: ' + (report.currentRoundTripTime * 1000).toFixed(1) + 'ms');
                    }
                }
                if (report.type === 'local-candidate' || report.type === 'remote-candidate') {
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
                }
            });
            this._log('========================');
        } catch (e) {
            this._logDebug('Failed to get stats: ' + e.message);
        }
    }

    /**
     * Add x-google bitrate hints to H264/video fmtp lines in SDP.
     * These are Chrome-specific but directly control the encoder's bitrate ramp-up.
     */
    addBitrateHints(sdp, minBitrateKbps, startBitrateKbps, maxBitrateKbps) {
        const lines = sdp.split('\r\n');
        const result = [];
        let inVideo = false;
        const metaPts = new Set();

        for (const line of lines) {
            if (line.startsWith('m=video')) inVideo = true;
            else if (line.startsWith('m=') && !line.startsWith('m=video')) inVideo = false;

            if (inVideo && line.startsWith('a=rtpmap:')) {
                const rest = line.substring(9);
                const parts = rest.split(' ');
                if (parts.length >= 2) {
                    const encoding = parts[1].toLowerCase();
                    if (encoding.startsWith('rtx/') || encoding.startsWith('red/') || encoding.startsWith('ulpfec/')) {
                        metaPts.add(parts[0]);
                    }
                }
            }
        }

        inVideo = false;
        for (const line of lines) {
            if (line.startsWith('m=video')) inVideo = true;
            else if (line.startsWith('m=') && !line.startsWith('m=video')) inVideo = false;

            if (inVideo && line.startsWith('a=fmtp:')) {
                const pt = line.split(':')[1].split(' ')[0];
                if (!metaPts.has(pt)) {
                    let modified = line;
                    if (!modified.includes('x-google-min-bitrate'))
                        modified += ';x-google-min-bitrate=' + minBitrateKbps;
                    if (!modified.includes('x-google-start-bitrate'))
                        modified += ';x-google-start-bitrate=' + startBitrateKbps;
                    if (!modified.includes('x-google-max-bitrate'))
                        modified += ';x-google-max-bitrate=' + maxBitrateKbps;
                    result.push(modified);
                    continue;
                }
            }
            result.push(line);
        }
        return result.join('\r\n');
    }

    /**
     * Set video bandwidth in SDP (b=AS: line).
     */
    setVideoBandwidth(sdp, kbps) {
        const lines = sdp.split('\r\n');
        const result = [];
        let inVideo = false;
        for (const line of lines) {
            if (line.startsWith('m=video')) {
                inVideo = true;
                result.push(line);
                result.push('b=AS:' + kbps);
                continue;
            }
            if (line.startsWith('m=') && !line.startsWith('m=video')) {
                inVideo = false;
            }
            if (inVideo && line.startsWith('b=')) continue;
            result.push(line);
        }
        return result.join('\r\n');
    }

    /**
     * Disconnect from the WHIP endpoint.
     */
    async disconnect() {
        this._logAlways('Disconnecting...');
        if (this.resourceUrl) {
            try {
                await fetch(this.resourceUrl, { method: 'DELETE' });
                this._log('Sent DELETE to resource URL');
            } catch (e) {
                this._logAlways('Failed to send DELETE: ' + e.message, 'error');
            }
        }
        this.cleanup();
        this._logAlways('Disconnected', 'success');
    }

    /**
     * Stop all local media tracks and close the peer connection.
     */
    cleanup() {
        if (this.localStream) {
            this.localStream.getTracks().forEach(t => t.stop());
            this.localStream = null;
        }
        if (this.peerConnection) {
            this.peerConnection.close();
            this.peerConnection = null;
        }
        this.resourceUrl = null;
        this.connected = false;
    }

    isConnected() {
        if (!this.peerConnection) return false;
        const s = this.peerConnection.iceConnectionState;
        return s === 'connected' || s === 'completed';
    }
}
