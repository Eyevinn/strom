// Unit tests for the WHIP ingest reconnect backoff.
//
// whip.js is a classic browser script; the `typeof module` guard at its foot is
// what makes it require()able here. The assertions read the production constants
// instead of restating them, so a wider MAX_RECONNECT_ATTEMPTS or an edited
// RECONNECT_DELAYS stays covered without touching this file.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const {
    ICE_DISCONNECT_GRACE_MS,
    RECONNECT_DELAYS,
    MAX_RECONNECT_ATTEMPTS,
    whipReconnectDelay,
} = require('../whip/whip.js');

// backend/src/blocks/builtin/whip.rs INACTIVITY_TIMEOUT. Mirrored by hand, since
// a JS test cannot read a Rust const: keep it in step with the Rust one.
const SERVER_INACTIVITY_TIMEOUT_MS = 10000;

test('every attempt within the retry budget has a delay', () => {
    for (let attempt = 1; attempt <= MAX_RECONNECT_ATTEMPTS; attempt++) {
        const delay = whipReconnectDelay(attempt);
        assert.equal(
            typeof delay, 'number',
            `attempt ${attempt} produced ${delay} instead of a number`,
        );
        assert.ok(Number.isFinite(delay) && delay > 0, `attempt ${attempt} -> ${delay}`);
    }
});

test('attempts past the end of the list clamp to the last delay', () => {
    const last = RECONNECT_DELAYS[RECONNECT_DELAYS.length - 1];
    for (const attempt of [RECONNECT_DELAYS.length, RECONNECT_DELAYS.length + 1, 100]) {
        assert.equal(whipReconnectDelay(attempt), last, `attempt ${attempt}`);
    }
});

test('the delays are the schedule the page advertises', () => {
    const schedule = [];
    for (let attempt = 1; attempt <= RECONNECT_DELAYS.length; attempt++) {
        schedule.push(whipReconnectDelay(attempt));
    }
    assert.deepEqual(schedule, RECONNECT_DELAYS);
});

test('the backoff grows', () => {
    // Strictly, not merely non-decreasing: a flat schedule satisfies every other
    // assertion here while retrying at a fixed interval forever.
    for (let attempt = 2; attempt <= MAX_RECONNECT_ATTEMPTS; attempt++) {
        assert.ok(
            whipReconnectDelay(attempt) >= whipReconnectDelay(attempt - 1),
            `attempt ${attempt} is shorter than attempt ${attempt - 1}`,
        );
    }
    assert.ok(
        whipReconnectDelay(MAX_RECONNECT_ATTEMPTS) > whipReconnectDelay(1),
        'the schedule is flat - it does not back off',
    );
});

test('the first retry beats the server freeing the slot', () => {
    // An abrupt drop stops media, and the server frees the ingest slot
    // SERVER_INACTIVITY_TIMEOUT_MS later. The page spends ICE_DISCONNECT_GRACE_MS
    // deciding the transport is dead and then waits for the first retry, so those
    // two together have to fit inside the server's window - otherwise the POST
    // always arrives after the slot is gone and the page can never get it back.
    const firstPost = ICE_DISCONNECT_GRACE_MS + whipReconnectDelay(1);
    assert.ok(
        firstPost < SERVER_INACTIVITY_TIMEOUT_MS,
        `first reconnect POST lands at ${firstPost}ms, at or past the server's ` +
        `${SERVER_INACTIVITY_TIMEOUT_MS}ms slot timeout`,
    );
});

test('the retry budget covers a long outage', () => {
    // Currently 331s across 15 attempts. The bound is loose on purpose: backing
    // off early must not shorten the total recovery window.
    let total = 0;
    for (let attempt = 1; attempt <= MAX_RECONNECT_ATTEMPTS; attempt++) {
        total += whipReconnectDelay(attempt);
    }
    assert.ok(total >= 300000, `retries span only ${total / 1000}s`);
});

test('the browser path still works', () => {
    // ingest.html pulls whip.js in with a plain <script src>, where `module` does
    // not exist. Run it the same way: the export guard at the foot has to stay
    // inert and the reconnect globals have to land in the page's scope. Separate
    // runInContext calls share one global lexical scope, as classic scripts on a
    // page do, so this is also how the inline script reaches these names.
    const src = fs.readFileSync(path.join(__dirname, '..', 'whip', 'whip.js'), 'utf8');
    const context = vm.createContext({ console });
    vm.runInContext(src, context);

    assert.equal(vm.runInContext('typeof module', context), 'undefined');
    assert.equal(vm.runInContext('typeof whipReconnectDelay', context), 'function');
    // Compared as JSON: the vm context has its own Array, and a cross-realm array
    // never passes a strict deep-equal.
    assert.equal(
        vm.runInContext('JSON.stringify(RECONNECT_DELAYS)', context),
        JSON.stringify(RECONNECT_DELAYS),
    );
    assert.equal(vm.runInContext('MAX_RECONNECT_ATTEMPTS', context), MAX_RECONNECT_ATTEMPTS);
});

test('ingest.html does not redeclare the shared reconnect globals', () => {
    // whip.js and the inline script share one global lexical scope, so a second
    // top-level declaration of either name is a parse error that takes the whole
    // page down rather than just the reconnect path.
    const html = fs.readFileSync(path.join(__dirname, '..', 'whip', 'ingest.html'), 'utf8');
    for (const name of ['RECONNECT_DELAYS', 'MAX_RECONNECT_ATTEMPTS']) {
        assert.ok(
            !new RegExp(`(?:const|let|var)\\s+${name}\\b`).test(html),
            `${name} is declared in ingest.html as well as whip.js`,
        );
    }
});
