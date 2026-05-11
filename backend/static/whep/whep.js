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
    constructor(endpoint, callbacks = {}, options = {}) {
        this.endpoint = endpoint;
        this.callbacks = callbacks;
        // Number of recvonly audio/video transceivers to add in the offer. The
        // server can answer with up to this many m= sections of each kind.
        this.numAudioTracks = Math.max(1, options.numAudioTracks || 1);
        this.numVideoTracks = Math.max(1, options.numVideoTracks || 1);
        this.peerConnection = null;
        this.resourceUrl = null;
        this.hasAudio = false;
        this.hasVideo = false;
        // Slot-indexed audio/video tracks: index N corresponds to transceiver N
        // (and therefore audio_in / audio_in_N on the backend pad). Slots that
        // have not yet received a track contain null. This is more robust than
        // arrival order, which relies on SDP m-line ordering being preserved.
        this.audioTracks = [];
        this.videoTracks = [];
        // Transceivers we created for audio/video, kept so we can map an
        // incoming ontrack event back to the slot we asked for.
        this.audioTransceivers = [];
        this.videoTransceivers = [];
        this.localCandidates = [];
        this.remoteCandidates = [];
        this.iceConfig = null;
        this.statsInterval = null;
        this._videoHealthInterval = null;
        this._statsOverlayInterval = null;
        this._prevFramesDecoded = 0;
        this._prevPacketsLost = 0;
        this._frozenSince = 0;      // timestamp when freeze was first detected
        this._lossRecoveryPending = false;
        // Previous snapshot for delta-based calculations (bitrate, fps, jitter buffer)
        this._prevStatsSnapshot = null;
        this._prevStatsTime = null;
        // EMA-smoothed jitter buffer delays (reduces noise from 1s delta windows)
        this._smoothedAudioJbuf = null;
        this._smoothedVideoJbuf = null;
        // Short session ID for log correlation
        this._sessionId = Array.from(crypto.getRandomValues(new Uint8Array(3)),
            b => b.toString(16).padStart(2, '0')).join('');
    }

    // Always log (for essential messages like errors, connection status)
    _logAlways(message, type = '') {
        const timestamp = new Date().toISOString();
        console.log(`[WHEP ${this._sessionId} ${timestamp}] ${message}`);
        if (this.callbacks.onLog) {
            this.callbacks.onLog(`[${this._sessionId}] ${message}`, type);
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
        console.log(`[WHEP DEBUG ${this._sessionId} ${timestamp}] ${message}`);
        if (this.callbacks.onLog) {
            this.callbacks.onLog(`[${this._sessionId}] [DEBUG] ${message}`, 'debug');
        }
    }

    async connect() {
        this.hasAudio = false;
        this.hasVideo = false;
        this.audioTracks = new Array(this.numAudioTracks).fill(null);
        this.videoTracks = new Array(this.numVideoTracks).fill(null);
        this.audioTransceivers = [];
        this.videoTransceivers = [];
        this.localCandidates = [];
        this.remoteCandidates = [];

        try {
            // Fetch ICE servers and transport policy from the server configuration
            let iceServers = [];
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
                this._log('ICE candidate error: ' + event.errorText +
                    ' (code=' + event.errorCode +
                    ' url=' + event.url +
                    ' host=' + event.address + ':' + event.port + ')');
            };

            this.peerConnection.ontrack = (event) => {
                this._log('Track received: kind=' + event.track.kind + ' id=' + event.track.id);
                if (event.track.kind === 'audio') {
                    this.hasAudio = true;
                    // Map back to the slot we asked for via the transceiver
                    // identity, so UI labels A0/A1/... line up with backend pads
                    // even if the answer SDP reorders m-sections.
                    let slot = this.audioTransceivers.indexOf(event.transceiver);
                    let isFirst;
                    if (slot >= 0 && slot < this.audioTracks.length) {
                        isFirst = this.audioTracks.every(t => t == null);
                        this.audioTracks[slot] = event.track;
                    } else {
                        // Fallback: transceiver not in our list — fill the first
                        // empty slot, otherwise append. Should not happen with
                        // a spec-compliant server.
                        this._log('Audio track from unknown transceiver, falling back to first empty slot', 'warning');
                        const firstEmpty = this.audioTracks.indexOf(null);
                        isFirst = this.audioTracks.every(t => t == null);
                        if (firstEmpty >= 0) {
                            this.audioTracks[firstEmpty] = event.track;
                        } else {
                            this.audioTracks.push(event.track);
                        }
                    }
                    // First audio track is delivered via onAudioTrack for backwards
                    // compatibility with single-audio UIs (stream cards).
                    if (isFirst && this.callbacks.onAudioTrack) {
                        this.callbacks.onAudioTrack(event.streams[0]);
                    }
                    // Notify on every audio track so multi-audio UIs can populate
                    // their selectors as tracks arrive. Array is slot-indexed and
                    // may contain null entries for slots not yet delivered.
                    if (this.callbacks.onAudioTracks) {
                        this.callbacks.onAudioTracks(this.audioTracks.slice());
                    }
                } else if (event.track.kind === 'video') {
                    this.hasVideo = true;
                    // Map back to the slot we asked for via the transceiver
                    // identity, so UI labels V0/V1/... line up with backend pads
                    // even if the answer SDP reorders m-sections.
                    let slot = this.videoTransceivers.indexOf(event.transceiver);
                    let isFirst;
                    if (slot >= 0 && slot < this.videoTracks.length) {
                        isFirst = this.videoTracks.every(t => t == null);
                        this.videoTracks[slot] = event.track;
                    } else {
                        this._log('Video track from unknown transceiver, falling back to first empty slot', 'warning');
                        const firstEmpty = this.videoTracks.indexOf(null);
                        isFirst = this.videoTracks.every(t => t == null);
                        if (firstEmpty >= 0) {
                            this.videoTracks[firstEmpty] = event.track;
                        } else {
                            this.videoTracks.push(event.track);
                        }
                    }
                    // First video track is delivered via onVideoTrack for
                    // backwards compatibility with single-video UIs.
                    if (isFirst && this.callbacks.onVideoTrack) {
                        this.callbacks.onVideoTrack(event.streams[0]);
                    }
                    // Notify on every video track so multi-video UIs can populate
                    // their selectors as tracks arrive. Array is slot-indexed and
                    // may contain null entries for slots not yet delivered.
                    if (this.callbacks.onVideoTracks) {
                        this.callbacks.onVideoTracks(this.videoTracks.slice());
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
                    this._logConnectionStats();
                    if (this.callbacks.onConnected) {
                        this.callbacks.onConnected();
                    }
                    this._updateStatus();
                    // Start periodic stats logging
                    this._startStatsLogging();
                    // Start video health monitor for freeze/artifact recovery
                    this._startVideoHealthMonitor();
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

            // Create transceivers - server decides what to send. Add
            // numAudioTracks + numVideoTracks recvonly transceivers so the
            // server can answer with that many m= sections of each kind. Keep
            // references so ontrack can map an incoming track to the slot we
            // asked for.
            this._log('Adding transceivers (audio x' + this.numAudioTracks +
                ' + video x' + this.numVideoTracks + ', recvonly)');
            this.audioTransceivers = [];
            for (let i = 0; i < this.numAudioTracks; i++) {
                this.audioTransceivers.push(
                    this.peerConnection.addTransceiver('audio', { direction: 'recvonly' })
                );
            }
            this.videoTransceivers = [];
            for (let i = 0; i < this.numVideoTracks; i++) {
                this.videoTransceivers.push(
                    this.peerConnection.addTransceiver('video', { direction: 'recvonly' })
                );
            }

            const offer = await this.peerConnection.createOffer();
            // Enable Opus stereo - Chrome defaults to mono which breaks stereo audio
            offer.sdp = enableOpusStereo(offer.sdp);
            await this.peerConnection.setLocalDescription(offer);

            this._log('Created SDP offer');
            this._logDebug('=== LOCAL SDP OFFER ===\n' + offer.sdp);

            // Wait for ICE gathering with detailed logging
            this._log('Waiting for ICE gathering (timeout: 1s)...');
            const gatheringStartTime = Date.now();

            await new Promise((resolve) => {
                if (this.peerConnection.iceGatheringState === 'complete') {
                    resolve();
                } else {
                    const timeout = setTimeout(() => {
                        this._log('ICE gathering timeout after 1s', 'warning');
                        resolve();
                    }, 1000);
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
            if (!whepDebugMode || !this.isConnected()) {
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

    // Monitor video health: detect freeze after packet loss and recover
    // by re-attaching the stream to force a fresh decoder context + PLI burst.
    _startVideoHealthMonitor() {
        if (this._videoHealthInterval) return;
        this._prevFramesDecoded = 0;
        this._prevPacketsLost = 0;
        this._frozenSince = 0;
        this._lossRecoveryPending = false;

        this._videoHealthInterval = setInterval(async () => {
            if (!this.isConnected() || !this.hasVideo) return;

            try {
                const stats = await this.peerConnection.getStats();
                let framesDecoded = 0;
                let packetsLost = 0;
                let pliCount = 0;

                stats.forEach(report => {
                    if (report.type === 'inbound-rtp' && report.kind === 'video') {
                        framesDecoded = report.framesDecoded || 0;
                        packetsLost = report.packetsLost || 0;
                        pliCount = report.pliCount || 0;
                    }
                });

                const newLoss = packetsLost - this._prevPacketsLost;
                const newFrames = framesDecoded - this._prevFramesDecoded;

                // Detect new packet loss
                if (newLoss > 0 && this._prevPacketsLost > 0) {
                    this._lossRecoveryPending = true;
                    this._logDebug('Video packet loss detected: +' + newLoss +
                        ' (total=' + packetsLost + ', PLIs sent=' + pliCount + ')');
                }

                // Check if video is frozen (no new frames decoded)
                const now = Date.now();
                if (this._prevFramesDecoded > 0 && newFrames === 0) {
                    if (this._frozenSince === 0) {
                        this._frozenSince = now;
                    }
                } else {
                    // Frames are being decoded
                    if (this._lossRecoveryPending && newFrames > 0) {
                        // Got fresh frames after loss - recovery succeeded
                        this._lossRecoveryPending = false;
                    }
                    this._frozenSince = 0;
                }

                // If frozen for >3s after packet loss, re-attach stream to recover
                if (this._frozenSince > 0 && this._lossRecoveryPending &&
                    (now - this._frozenSince) > 3000) {
                    this._logAlways('Video frozen after packet loss, requesting recovery', 'warning');
                    this._recoverVideo();
                    this._frozenSince = 0;
                    this._lossRecoveryPending = false;
                }

                this._prevFramesDecoded = framesDecoded;
                this._prevPacketsLost = packetsLost;
            } catch (e) {
                // PC may be closing
            }
        }, 1000);
    }

    _stopVideoHealthMonitor() {
        if (this._videoHealthInterval) {
            clearInterval(this._videoHealthInterval);
            this._videoHealthInterval = null;
        }
    }

    // Re-attach video stream to force decoder reset + new PLI
    _recoverVideo() {
        if (!this.peerConnection) return;
        const receivers = this.peerConnection.getReceivers();
        for (const receiver of receivers) {
            if (receiver.track && receiver.track.kind === 'video') {
                // Create a new MediaStream with the same track to force
                // the video element to re-initialize its decoder, which
                // triggers an immediate PLI request for a fresh keyframe.
                const freshStream = new MediaStream([receiver.track]);
                if (this.callbacks.onVideoTrack) {
                    this.callbacks.onVideoTrack(freshStream);
                }
                this._logDebug('Re-attached video stream for decoder recovery');
                break;
            }
        }
    }

    // Collect detailed WebRTC stats for the overlay panel.
    // Returns a structured object with audio, video, and transport metrics.
    async getDetailedStats() {
        if (!this.peerConnection) return null;

        try {
            const stats = await this.peerConnection.getStats();
            const now = performance.now();
            const result = { audio: null, video: null, transport: null };

            let audioBytesReceived = 0;
            let videoBytesReceived = 0;
            let audioJitterBufferDelay = 0;
            let audioJitterBufferEmitted = 0;
            let videoJitterBufferDelay = 0;
            let videoJitterBufferEmitted = 0;

            stats.forEach(report => {
                if (report.type === 'inbound-rtp' && report.kind === 'audio') {
                    audioBytesReceived = report.bytesReceived || 0;
                    audioJitterBufferDelay = report.jitterBufferDelay || 0;
                    audioJitterBufferEmitted = report.jitterBufferEmittedCount || 0;
                    result.audio = {
                        jitter: report.jitter != null ? report.jitter * 1000 : null, // ms
                        jitterBufferDelay: null, // computed below from deltas
                        packetsReceived: report.packetsReceived || 0,
                        packetsLost: report.packetsLost || 0,
                        codec: null,
                        bitrate: null,
                        bytesReceived: audioBytesReceived,
                        codecId: report.codecId || null,
                        // Raw cumulative values for delta-based jitter buffer calculation
                        _jbufDelayCum: audioJitterBufferDelay,
                        _jbufEmittedCum: audioJitterBufferEmitted,
                    };
                }

                if (report.type === 'inbound-rtp' && report.kind === 'video') {
                    videoBytesReceived = report.bytesReceived || 0;
                    videoJitterBufferDelay = report.jitterBufferDelay || 0;
                    videoJitterBufferEmitted = report.jitterBufferEmittedCount || 0;
                    result.video = {
                        jitter: report.jitter != null ? report.jitter * 1000 : null, // ms
                        jitterBufferDelay: null, // computed below from deltas
                        packetsReceived: report.packetsReceived || 0,
                        packetsLost: report.packetsLost || 0,
                        framesDecoded: report.framesDecoded || 0,
                        framesDropped: report.framesDropped || 0,
                        framesReceived: report.framesReceived || 0,
                        frameWidth: report.frameWidth || null,
                        frameHeight: report.frameHeight || null,
                        nackCount: report.nackCount || 0,
                        pliCount: report.pliCount || 0,
                        firCount: report.firCount || 0,
                        codec: null,
                        bitrate: null,
                        fps: null,
                        bytesReceived: videoBytesReceived,
                        codecId: report.codecId || null,
                        // Raw cumulative values for delta-based jitter buffer calculation
                        _jbufDelayCum: videoJitterBufferDelay,
                        _jbufEmittedCum: videoJitterBufferEmitted,
                    };
                }

                if (report.type === 'candidate-pair' && report.state === 'succeeded' && report.nominated) {
                    result.transport = {
                        rtt: report.currentRoundTripTime != null
                            ? report.currentRoundTripTime * 1000 : null, // ms
                    };
                }
            });

            // Resolve codec names from codec reports
            stats.forEach(report => {
                if (report.type === 'codec') {
                    if (result.audio && result.audio.codecId === report.id) {
                        result.audio.codec = report.mimeType ? report.mimeType.replace('audio/', '') : null;
                    }
                    if (result.video && result.video.codecId === report.id) {
                        result.video.codec = report.mimeType ? report.mimeType.replace('video/', '') : null;
                    }
                }
            });

            // Calculate delta-based metrics (bitrate, fps, jitter buffer) from previous snapshot.
            // jitterBufferDelay and jitterBufferEmittedCount are cumulative counters —
            // dividing cumulative totals gives a session-wide average dominated by initial
            // buffering. Delta-based calculation shows the actual current jitter buffer level.
            // EMA smoothing (alpha=0.3) reduces noise from 1-second delta windows.
            const EMA_ALPHA = 0.3;
            if (this._prevStatsSnapshot && this._prevStatsTime) {
                const dt = (now - this._prevStatsTime) / 1000; // seconds
                if (dt > 0) {
                    if (result.audio && this._prevStatsSnapshot.audio) {
                        const dBytes = result.audio.bytesReceived - this._prevStatsSnapshot.audio.bytesReceived;
                        result.audio.bitrate = Math.round((dBytes * 8) / (dt * 1000)); // kbps
                        const dJbuf = result.audio._jbufDelayCum - this._prevStatsSnapshot.audio._jbufDelayCum;
                        const dEmitted = result.audio._jbufEmittedCum - this._prevStatsSnapshot.audio._jbufEmittedCum;
                        if (dEmitted > 0) {
                            const raw = (dJbuf / dEmitted) * 1000; // ms
                            this._smoothedAudioJbuf = this._smoothedAudioJbuf != null
                                ? EMA_ALPHA * raw + (1 - EMA_ALPHA) * this._smoothedAudioJbuf
                                : raw;
                            result.audio.jitterBufferDelay = this._smoothedAudioJbuf;
                        }
                    }
                    if (result.video && this._prevStatsSnapshot.video) {
                        const dBytes = result.video.bytesReceived - this._prevStatsSnapshot.video.bytesReceived;
                        result.video.bitrate = Math.round((dBytes * 8) / (dt * 1000)); // kbps
                        const dFrames = result.video.framesDecoded - this._prevStatsSnapshot.video.framesDecoded;
                        result.video.fps = Math.round(dFrames / dt);
                        const dJbuf = result.video._jbufDelayCum - this._prevStatsSnapshot.video._jbufDelayCum;
                        const dEmitted = result.video._jbufEmittedCum - this._prevStatsSnapshot.video._jbufEmittedCum;
                        if (dEmitted > 0) {
                            const raw = (dJbuf / dEmitted) * 1000; // ms
                            this._smoothedVideoJbuf = this._smoothedVideoJbuf != null
                                ? EMA_ALPHA * raw + (1 - EMA_ALPHA) * this._smoothedVideoJbuf
                                : raw;
                            result.video.jitterBufferDelay = this._smoothedVideoJbuf;
                        }
                    }
                }
            }

            // A/V sync estimate: difference in jitter buffer delays
            if (result.audio && result.video &&
                result.audio.jitterBufferDelay != null && result.video.jitterBufferDelay != null) {
                result.avSyncOffset = result.audio.jitterBufferDelay - result.video.jitterBufferDelay; // ms
            } else {
                result.avSyncOffset = null;
            }

            this._prevStatsSnapshot = result;
            this._prevStatsTime = now;

            return result;
        } catch (e) {
            return null;
        }
    }

    // Start periodic stats collection for the overlay panel.
    // Calls onStats callback every second with detailed metrics.
    startStatsOverlay() {
        this.stopStatsOverlay();
        // Reset delta baseline so first reading gets bitrate on second tick
        this._prevStatsSnapshot = null;
        this._prevStatsTime = null;
        this._smoothedAudioJbuf = null;
        this._smoothedVideoJbuf = null;
        this._statsRelayTick = 0;

        this._statsOverlayInterval = setInterval(async () => {
            if (!this.isConnected()) return;
            const stats = await this.getDetailedStats();
            if (stats && this.callbacks.onStats) {
                this.callbacks.onStats(stats);
            }
            // Relay full stats to server every 5s (only when debug is also enabled)
            this._statsRelayTick++;
            if (stats && whepDebugMode && this._statsRelayTick % 5 === 0) {
                this._relayStats(stats);
            }
        }, 1000);
    }

    stopStatsOverlay() {
        if (this._statsOverlayInterval) {
            clearInterval(this._statsOverlayInterval);
            this._statsOverlayInterval = null;
        }
    }

    // Relay structured media stats to the server via /api/client-log.
    // Sent as a single JSON-formatted log line so the server can parse it.
    _relayStats(stats) {
        const s = {};
        if (stats.audio) {
            s.audio = {
                bitrate_kbps: stats.audio.bitrate,
                jitter_buffer_ms: stats.audio.jitterBufferDelay != null ? Math.round(stats.audio.jitterBufferDelay) : null,
                jitter_ms: stats.audio.jitter != null ? +stats.audio.jitter.toFixed(1) : null,
                packets_received: stats.audio.packetsReceived,
                packets_lost: stats.audio.packetsLost,
                codec: stats.audio.codec,
            };
        }
        if (stats.video) {
            s.video = {
                bitrate_kbps: stats.video.bitrate,
                jitter_buffer_ms: stats.video.jitterBufferDelay != null ? Math.round(stats.video.jitterBufferDelay) : null,
                jitter_ms: stats.video.jitter != null ? +stats.video.jitter.toFixed(1) : null,
                packets_received: stats.video.packetsReceived,
                packets_lost: stats.video.packetsLost,
                codec: stats.video.codec,
                resolution: stats.video.frameWidth && stats.video.frameHeight
                    ? stats.video.frameWidth + 'x' + stats.video.frameHeight : null,
                fps: stats.video.fps,
                frames_dropped: stats.video.framesDropped,
                nack_count: stats.video.nackCount,
                pli_count: stats.video.pliCount,
            };
        }
        if (stats.transport) {
            s.rtt_ms = stats.transport.rtt != null ? +stats.transport.rtt.toFixed(1) : null;
        }
        if (stats.avSyncOffset != null) {
            s.av_sync_offset_ms = Math.round(stats.avSyncOffset);
        }
        fetch('/api/client-log', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify([{ msg: '[MEDIA-STATS] ' + JSON.stringify(s), level: 'info' }]),
        }).catch(() => {});
    }

    _updateStatus() {
        if (!this.peerConnection) return;
        const state = this.peerConnection.iceConnectionState;
        if (state === 'connected' || state === 'completed') {
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
        this._stopVideoHealthMonitor();
        this.stopStatsOverlay();

        if (this.peerConnection) {
            this.peerConnection.close();
            this.peerConnection = null;
        }
        this.resourceUrl = null;
        this.hasAudio = false;
        this.hasVideo = false;
        this.audioTracks = [];
        this.videoTracks = [];
        this.audioTransceivers = [];
        this.videoTransceivers = [];
    }

    isConnected() {
        if (!this.peerConnection) return false;
        const s = this.peerConnection.iceConnectionState;
        return s === 'connected' || s === 'completed';
    }

    /**
     * Get info about the active ICE transport (candidate type, remote address).
     * @returns {{ type: string, protocol: string, remoteAddress: string, relayProtocol: string|null } | null}
     */
    async getTransportInfo() {
        if (!this.peerConnection) return null;
        try {
            const stats = await this.peerConnection.getStats();
            // Find the active candidate pair via the transport report's selectedCandidatePairId
            let selectedPairId = null;
            stats.forEach(report => {
                if (report.type === 'transport' && report.selectedCandidatePairId) {
                    selectedPairId = report.selectedCandidatePairId;
                }
            });
            let localId = null;
            let remoteId = null;
            if (selectedPairId) {
                const pair = stats.get(selectedPairId);
                if (pair) {
                    localId = pair.localCandidateId;
                    remoteId = pair.remoteCandidateId;
                }
            }
            if (!localId) return null;
            const local = stats.get(localId);
            const remote = stats.get(remoteId);
            const remoteAddr = remote?.address || remote?.ip || null;
            console.log('[WHEP-TRANSPORT] selectedPairId=' + selectedPairId +
                ' local=' + JSON.stringify(local) +
                ' remote=' + JSON.stringify(remote));
            return {
                type: local?.candidateType || 'unknown',
                protocol: local?.protocol || 'unknown',
                remoteAddress: remoteAddr ? remoteAddr + ':' + remote.port : null,
                relayProtocol: local?.relayProtocol || null,
            };
        } catch (e) {
            return null;
        }
    }
}

// UI Helper functions (setStatus, setElementClass, copyFallback are in webrtc.js)

function createAudioIndicator(endpointId) {
    return `<div class="meter-bar"><div class="meter-fill" id="meter-fill-${endpointId}"></div></div>`;
}

function escapeHtml(str) {
    const div = document.createElement('div');
    div.textContent = str;
    return div.innerHTML;
}
