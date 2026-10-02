// Unit tests for resolving a WHIP/WHEP Location header to the session resource.
//
// The DELETE that ends a session goes to this URL, so a Location resolved against
// the wrong server leaves the real session running.

const test = require('node:test');
const assert = require('node:assert/strict');

const { resolveResourceUrl } = require('../webrtc/webrtc.js');

const PAGE = 'https://studio.example.com/player/whep';

test('a path Location belongs to the endpoint server, not the page server', () => {
    assert.equal(
        resolveResourceUrl('/whep/program/resource/abc', 'https://strom.example.net:8443/whep/program', PAGE),
        'https://strom.example.net:8443/whep/program/resource/abc',
    );
});

test('a same-origin endpoint given as a path resolves against the page', () => {
    assert.equal(
        resolveResourceUrl('/whip/cam1/resource/abc', '/whip/cam1', PAGE),
        'https://studio.example.com/whip/cam1/resource/abc',
    );
});

test('an absolute Location is kept', () => {
    assert.equal(
        resolveResourceUrl('https://media.example.org/r/abc', 'https://strom.example.net/whep/program', PAGE),
        'https://media.example.org/r/abc',
    );
});

test('no Location gives no resource URL', () => {
    assert.equal(resolveResourceUrl(null, 'https://strom.example.net/whep/program', PAGE), null);
});
