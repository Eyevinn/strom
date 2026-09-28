// Unit tests for the status note the vision mixer page shows after a take.
//
// The page script is inline in vision-mixer.html and DOM-driven, so it cannot
// be require()d. takeStatusNote is a pure function; its source is cut out of
// the page by brace matching and evaluated on its own, so the test runs the
// function the page ships rather than a copy.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function loadTakeStatusNote() {
    const html = fs.readFileSync(path.join(__dirname, '..', 'vision-mixer.html'), 'utf8');
    const start = html.indexOf('function takeStatusNote(');
    assert.notEqual(start, -1, 'takeStatusNote not found in vision-mixer.html');
    let depth = 0;
    let end = html.indexOf('{', start);
    do {
        if (html[end] === '{') depth++;
        else if (html[end] === '}') depth--;
        end++;
    } while (depth > 0);
    const ctx = {};
    vm.runInNewContext(html.slice(start, end) + '\nthis.takeStatusNote = takeStatusNote;', ctx);
    return ctx.takeStatusNote;
}

const takeStatusNote = loadTakeStatusNote();

test('a take that ran as requested adds no note', () => {
    assert.equal(takeStatusNote('fade', 'fade'), '');
    assert.equal(takeStatusNote('SLIDE_LEFT', 'slide_left'), '');
    assert.equal(takeStatusNote('fade', undefined), '');
});

test('a fade that morphed is not called a downgrade', () => {
    const note = takeStatusNote('fade', 'morph');
    assert.match(note, /ran as a move/);
    assert.doesNotMatch(note, /downgrad/);
});

test('the morph note fits a punch-in, where the box stays put and the crop changes', () => {
    for (const requested of ['fade', 'slide_left']) {
        const note = takeStatusNote(requested, 'morph');
        assert.match(note, /zoom/);
        assert.doesNotMatch(note, /slid/);
    }
});

for (const requested of ['slide_left', 'push_left', 'wipe_left']) {
    test(`a ${requested} that morphed says it was downgraded`, () => {
        assert.match(takeStatusNote(requested, 'morph'), /downgraded to morph/);
    });
}

test('any other substitution names what ran', () => {
    assert.match(takeStatusNote('slide_left', 'fade'), /downgraded to fade/);
});
