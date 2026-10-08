// Unit tests for WhepConnection's session lifecycle: an ICE drop that recovers,
// one that does not, and a Disconnect while the WHEP POST is in flight.
//
// whep.js is a classic browser script that leans on webrtc.js globals, so both
// are run in one vm context the way the player page loads them. RTCPeerConnection
// and fetch are fakes: the tests drive ICE states and resolve the POST by hand.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const STATIC = path.join(__dirname, '..');
const ENDPOINT = 'https://strom.example.com/whep/program';
const RESOURCE = 'https://strom.example.com/whep/program/resource/abc';

class FakePeerConnection {
    constructor() {
        this.iceConnectionState = 'new';
        this.iceGatheringState = 'complete';
        this.connectionState = 'new';
        this.signalingState = 'stable';
        this.localDescription = null;
        this.closed = false;
    }
    addTransceiver() { return {}; }
    async createOffer() { return { type: 'offer', sdp: 'v=0\r\n' }; }
    async setLocalDescription(desc) { this.localDescription = desc; }
    async setRemoteDescription() {}
    async getStats() { return new Map(); }
    close() { this.closed = true; }
    setIceState(state) {
        this.iceConnectionState = state;
        this.oniceconnectionstatechange();
    }
}

// Load webrtc.js + whep.js into a fresh page-like context. `post` decides when
// the WHEP POST resolves; every request is recorded in `requests`.
function loadPage(post) {
    const requests = [];
    const peerConnections = [];
    const fetch = async (url, opts = {}) => {
        const method = opts.method || 'GET';
        requests.push({ url, method });
        if (url === '/api/ice-servers') {
            return { ok: true, json: async () => ({}) };
        }
        if (method === 'POST') return post();
        return { ok: true, text: async () => '' };
    };
    const context = vm.createContext({
        console: { log() {}, error() {} },
        URL,
        crypto: globalThis.crypto,
        location: { href: 'https://strom.example.com/player/whep' },
        fetch,
        RTCPeerConnection: class extends FakePeerConnection {
            constructor(...args) {
                super(...args);
                peerConnections.push(this);
            }
        },
        // Looked up at call time so node:test's mock timers apply.
        setTimeout: (...a) => setTimeout(...a),
        clearTimeout: (...a) => clearTimeout(...a),
        setInterval: (...a) => setInterval(...a),
        clearInterval: (...a) => clearInterval(...a),
    });
    for (const file of ['webrtc/webrtc.js', 'whep/whep.js']) {
        vm.runInContext(fs.readFileSync(path.join(STATIC, file), 'utf8'), context);
    }
    return {
        requests,
        peerConnections,
        WhepConnection: vm.runInContext('WhepConnection', context),
        graceMs: vm.runInContext('ICE_DISCONNECT_GRACE_MS', context),
    };
}

function answer() {
    return {
        ok: true,
        headers: { get: (name) => (name === 'Location' ? RESOURCE : null) },
        text: async () => 'v=0\r\n',
    };
}

const deletes = (requests) => requests.filter((r) => r.method === 'DELETE');

async function connected(page, callbacks) {
    const conn = new page.WhepConnection(ENDPOINT, callbacks);
    assert.equal(await conn.connect(), true);
    const pc = page.peerConnections[0];
    pc.setIceState('connected');
    return { conn, pc };
}

test('an ICE drop that recovers keeps the session and does not tear down', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
    const page = loadPage(async () => answer());
    let disconnected = 0;
    const { conn, pc } = await connected(page, { onDisconnected: () => disconnected++ });

    pc.setIceState('disconnected');
    t.mock.timers.tick(page.graceMs - 1);
    pc.setIceState('connected');
    t.mock.timers.tick(page.graceMs * 2);

    assert.equal(disconnected, 0, 'onDisconnected fired for a drop that recovered');
    assert.equal(pc.closed, false);
    assert.equal(conn.peerConnection, pc);
    assert.equal(deletes(page.requests).length, 0);
    conn.close();
});

test('an ICE drop that does not recover closes the PeerConnection and ends the session', async (t) => {
    t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
    const page = loadPage(async () => answer());
    let disconnected = 0;
    const { conn, pc } = await connected(page, { onDisconnected: () => disconnected++ });

    pc.setIceState('disconnected');
    t.mock.timers.tick(page.graceMs);

    assert.equal(disconnected, 1);
    // Closed, so a late recovery cannot bring back a session the player has
    // already detached its media from.
    assert.equal(pc.closed, true);
    assert.equal(conn.peerConnection, null);
    assert.deepEqual(deletes(page.requests).map((r) => r.url), [RESOURCE]);
});

test('Disconnect during the POST deletes the session the POST creates', async () => {
    let resolvePost;
    const page = loadPage(() => new Promise((resolve) => { resolvePost = resolve; }));
    let errors = 0;
    const conn = new page.WhepConnection(ENDPOINT, { onError: () => errors++ });

    const connecting = conn.connect();
    while (!resolvePost) await new Promise((r) => setImmediate(r));
    await conn.disconnect();
    assert.equal(deletes(page.requests).length, 0, 'no resource URL is known yet');

    resolvePost(answer());
    assert.equal(await connecting, false);
    assert.deepEqual(deletes(page.requests).map((r) => r.url), [RESOURCE]);
    assert.equal(errors, 0, 'an abort is not a connection error');
    assert.equal(conn.peerConnection, null);
});

test('Disconnect before the PeerConnection exists sends no POST', async () => {
    const page = loadPage(async () => answer());
    const conn = new page.WhepConnection(ENDPOINT, {});
    const connecting = conn.connect();
    conn.close(); // lands during the /api/ice-servers await
    assert.equal(await connecting, false);
    assert.equal(page.peerConnections.length, 0, 'a PeerConnection was built after close()');
    assert.equal(page.requests.filter((r) => r.method === 'POST').length, 0);
});
