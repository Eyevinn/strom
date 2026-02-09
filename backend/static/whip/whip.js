/**
 * WHIP Client - WebRTC-HTTP Ingestion Protocol client for browser-based sending.
 *
 * Sends camera/microphone media to a WHIP server endpoint.
 */

class WhipClient {
    /**
     * @param {string} endpointUrl - The WHIP endpoint URL (e.g., /whip/my-stream)
     * @param {Object} callbacks - Event callbacks
     */
    constructor(endpointUrl, callbacks = {}) {
        this.endpointUrl = endpointUrl;
        this.callbacks = callbacks;
        this.pc = null;
        this.resourceUrl = null;
        this.localStream = null;
        this.iceServers = [];
        this.connected = false;
    }

    log(msg, type = 'info') {
        if (this.callbacks.onLog) {
            this.callbacks.onLog(msg, type);
        }
    }

    /**
     * Fetch ICE server configuration from the backend.
     */
    async fetchIceServers() {
        try {
            const resp = await fetch('/api/ice-servers');
            if (resp.ok) {
                const data = await resp.json();
                if (data && data.length > 0) {
                    this.iceServers = data.map(url => {
                        if (url.startsWith('turn:') || url.startsWith('turns:')) {
                            return { urls: url, username: '', credential: '' };
                        }
                        return { urls: url };
                    });
                    this.log('Loaded ICE servers: ' + data.join(', '));
                }
            }
        } catch (e) {
            this.log('Failed to fetch ICE servers: ' + e.message, 'error');
        }
    }

    /**
     * Get user media (camera/microphone).
     * @param {Object} constraints - getUserMedia constraints
     * @returns {MediaStream}
     */
    async getMedia(constraints) {
        this.log('Requesting user media...');
        if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
            const msg = window.isSecureContext
                ? 'getUserMedia not supported by this browser'
                : 'Camera/mic requires HTTPS. Connect via localhost or enable HTTPS.';
            this.log(msg, 'error');
            throw new Error(msg);
        }
        try {
            this.localStream = await navigator.mediaDevices.getUserMedia(constraints);
            const tracks = this.localStream.getTracks().map(t => t.kind + ':' + t.label);
            this.log('Got media tracks: ' + tracks.join(', '));
            return this.localStream;
        } catch (e) {
            this.log('Failed to get user media: ' + e.message, 'error');
            throw e;
        }
    }

    /**
     * Connect to the WHIP endpoint and start sending media.
     * @param {MediaStream} stream - The media stream to send
     */
    async connect(stream) {
        if (this.connected) {
            this.log('Already connected, disconnect first', 'error');
            return;
        }

        await this.fetchIceServers();

        this.log('Creating peer connection...');

        const config = {
            iceServers: this.iceServers.length > 0 ? this.iceServers : undefined,
            bundlePolicy: 'max-bundle',
        };

        this.pc = new RTCPeerConnection(config);

        this.pc.oniceconnectionstatechange = () => {
            if (!this.pc) return;
            const state = this.pc.iceConnectionState;
            this.log('ICE connection state: ' + state);
            if (this.callbacks.onIceState) {
                this.callbacks.onIceState(state);
            }
            if (state === 'connected' || state === 'completed') {
                this.connected = true;
                if (this.callbacks.onConnected) {
                    this.callbacks.onConnected();
                }
            } else if (state === 'disconnected' || state === 'failed') {
                this.connected = false;
                if (this.callbacks.onDisconnected) {
                    this.callbacks.onDisconnected();
                }
            }
        };

        this.pc.onconnectionstatechange = () => {
            if (!this.pc) return;
            const state = this.pc.connectionState;
            this.log('Connection state: ' + state);
            // 'failed' means DTLS/ICE has failed unrecoverably
            if (state === 'failed' && this.connected) {
                this.connected = false;
                if (this.callbacks.onDisconnected) {
                    this.callbacks.onDisconnected();
                }
            }
        };

        // Add tracks from the stream
        for (const track of stream.getTracks()) {
            this.log('Adding ' + track.kind + ' track: ' + track.label);
            const transceiver = this.pc.addTransceiver(track, { direction: 'sendonly' });

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
                    this.log('Set video encoding: maxBitrate=4Mbps, maxFramerate=30');
                } catch (e) {
                    this.log('Failed to set video encoding params: ' + e.message, 'error');
                }
            }
        }

        // Create SDP offer
        this.log('Creating SDP offer...');
        const offer = await this.pc.createOffer();

        // Enable stereo for Opus if present
        offer.sdp = this.enableOpusStereo(offer.sdp);
        // Note: bitrate hints (x-google-*) and b=AS are handled server-side
        // by fix_video_bitrate_hints() on the SDP answer. No need to add them here.

        // Log SDP offer details
        this.log('SDP offer key lines:');
        for (const line of offer.sdp.split('\r\n')) {
            if (line.startsWith('m=video') || line.startsWith('b=AS') ||
                line.startsWith('a=rtpmap:') || line.startsWith('a=fmtp:') ||
                line.startsWith('a=extmap:') ||
                line.includes('transport-cc') || line.includes('remb') ||
                line.includes('x-google')) {
                this.log('  ' + line);
            }
        }

        await this.pc.setLocalDescription(offer);

        // Wait for ICE gathering to complete (or timeout)
        await this.waitForIceGathering(5000);

        const finalOffer = this.pc.localDescription;
        this.log('Sending SDP offer to ' + this.endpointUrl);

        // POST the offer to the WHIP endpoint.
        // Retry on 500 errors - whipserversrc may need time to clean up after
        // a previous session before it can accept a new one.
        const maxRetries = 3;
        const retryDelayMs = 2000;
        let response;
        for (let attempt = 1; attempt <= maxRetries; attempt++) {
            try {
                response = await fetch(this.endpointUrl, {
                    method: 'POST',
                    headers: {
                        'Content-Type': 'application/sdp',
                    },
                    body: finalOffer.sdp,
                });
            } catch (e) {
                this.log('Failed to send offer: ' + e.message, 'error');
                this.cleanup();
                if (this.callbacks.onError) {
                    this.callbacks.onError('Failed to connect: ' + e.message);
                }
                return;
            }

            if (response.ok) break;

            if (response.status >= 500 && attempt < maxRetries) {
                this.log('Server returned ' + response.status + ', retrying in ' + (retryDelayMs / 1000) + 's (attempt ' + attempt + '/' + maxRetries + ')...');
                await new Promise(r => setTimeout(r, retryDelayMs));
                continue;
            }

            const errorText = await response.text();
            this.log('WHIP server returned ' + response.status + ': ' + errorText, 'error');
            this.cleanup();
            if (this.callbacks.onError) {
                this.callbacks.onError('Server error: ' + response.status);
            }
            return;
        }

        // Get the resource URL for cleanup
        const locationHeader = response.headers.get('Location');
        if (locationHeader) {
            // Make it absolute if relative
            if (locationHeader.startsWith('/')) {
                this.resourceUrl = window.location.origin + locationHeader;
            } else {
                this.resourceUrl = locationHeader;
            }
            this.log('Resource URL: ' + this.resourceUrl);
        }

        // Set the SDP answer
        const answerSdp = await response.text();
        this.log('Received SDP answer (' + answerSdp.length + ' bytes)');

        try {
            await this.pc.setRemoteDescription({
                type: 'answer',
                sdp: answerSdp,
            });
            this.log('Remote description set', 'success');

            // Log key SDP answer lines
            this.log('SDP answer key lines:');
            for (const line of answerSdp.split('\n')) {
                const l = line.trim();
                if (l.startsWith('m=video') || l.startsWith('b=') ||
                    l.startsWith('a=rtpmap:') || l.startsWith('a=fmtp:') ||
                    l.includes('transport-cc') || l.includes('remb') ||
                    l.includes('rtcp-fb') || l.startsWith('a=extmap:')) {
                    this.log('  ' + l);
                }
            }
        } catch (e) {
            this.log('Failed to set remote description: ' + e.message, 'error');
            this.cleanup();
            if (this.callbacks.onError) {
                this.callbacks.onError('Failed to set answer: ' + e.message);
            }
        }
    }

    /**
     * Wait for ICE gathering to complete or timeout.
     */
    waitForIceGathering(timeoutMs) {
        return new Promise((resolve) => {
            if (this.pc.iceGatheringState === 'complete') {
                resolve();
                return;
            }

            const timeout = setTimeout(() => {
                this.log('ICE gathering timeout, proceeding with gathered candidates');
                resolve();
            }, timeoutMs);

            this.pc.onicegatheringstatechange = () => {
                if (this.pc.iceGatheringState === 'complete') {
                    clearTimeout(timeout);
                    this.log('ICE gathering complete');
                    resolve();
                }
            };
        });
    }

    /**
     * Enable stereo for Opus codec in SDP.
     */
    enableOpusStereo(sdp) {
        return sdp.replace(
            /a=fmtp:(\d+) (.+)/g,
            (match, pt, params) => {
                if (params.includes('opus')) {
                    if (!params.includes('stereo=')) {
                        params += ';stereo=1';
                    }
                    if (!params.includes('sprop-stereo=')) {
                        params += ';sprop-stereo=1';
                    }
                }
                return 'a=fmtp:' + pt + ' ' + params;
            }
        );
    }

    /**
     * Add x-google bitrate hints to H264/video fmtp lines in SDP.
     * These are Chrome-specific but directly control the encoder's bitrate ramp-up.
     */
    addBitrateHints(sdp, minBitrateKbps, startBitrateKbps, maxBitrateKbps) {
        // Add x-google hints only to actual video codec fmtp lines (not RTX/RED/ULPFEC)
        const lines = sdp.split('\r\n');
        const result = [];
        let inVideo = false;
        const metaPts = new Set(); // RTX, RED, ULPFEC payload types to skip

        // First pass: find RTX/RED/ULPFEC payload types in video section
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

        // Second pass: add bitrate hints to non-meta fmtp lines in video section
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
     * Set video bandwidth in SDP (b=AS: line) to hint higher initial bitrate.
     * @param {string} sdp
     * @param {number} kbps - bandwidth in kbps
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
            // Skip existing b= lines in video section
            if (inVideo && line.startsWith('b=')) continue;
            result.push(line);
        }
        return result.join('\r\n');
    }

    /**
     * Disconnect from the WHIP endpoint.
     */
    async disconnect() {
        this.log('Disconnecting...');

        // Send DELETE to resource URL
        if (this.resourceUrl) {
            try {
                await fetch(this.resourceUrl, { method: 'DELETE' });
                this.log('Sent DELETE to resource URL');
            } catch (e) {
                this.log('Failed to send DELETE: ' + e.message, 'error');
            }
        }

        this.cleanup();
        this.log('Disconnected', 'success');
    }

    /**
     * Stop all local media tracks and close the peer connection.
     */
    cleanup() {
        if (this.localStream) {
            this.localStream.getTracks().forEach(t => t.stop());
            this.localStream = null;
        }

        if (this.pc) {
            this.pc.close();
            this.pc = null;
        }

        this.resourceUrl = null;
        this.connected = false;
    }
}
