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
            } else if (state === 'disconnected' || state === 'failed' || state === 'closed') {
                this.connected = false;
                if (this.callbacks.onDisconnected) {
                    this.callbacks.onDisconnected();
                }
            }
        };

        this.pc.onconnectionstatechange = () => {
            this.log('Connection state: ' + this.pc.connectionState);
        };

        // Add tracks from the stream
        for (const track of stream.getTracks()) {
            this.log('Adding ' + track.kind + ' track: ' + track.label);
            if (track.kind === 'video') {
                this.pc.addTransceiver(track, { direction: 'sendonly' });
            } else {
                this.pc.addTransceiver(track, { direction: 'sendonly' });
            }
        }

        // Create SDP offer
        this.log('Creating SDP offer...');
        const offer = await this.pc.createOffer();

        // Enable stereo for Opus if present
        offer.sdp = this.enableOpusStereo(offer.sdp);

        await this.pc.setLocalDescription(offer);

        // Wait for ICE gathering to complete (or timeout)
        await this.waitForIceGathering(5000);

        const finalOffer = this.pc.localDescription;
        this.log('Sending SDP offer to ' + this.endpointUrl);

        // POST the offer to the WHIP endpoint
        let response;
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

        if (!response.ok) {
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
