import test from 'node:test';
import assert from 'node:assert/strict';

import { koReindexNotice } from '../src/lib/koReindex.js';

test('koReindexNotice is null when nothing needs reindexing', () => {
	assert.equal(koReindexNotice(null, ['C:\\a']), null);
	assert.equal(koReindexNotice(undefined, []), null);
	assert.equal(koReindexNotice({ available: true, roots: [] }, ['C:\\a']), null);
});

test('koReindexNotice picks message kind from current dictionary availability', () => {
	assert.deepEqual(koReindexNotice({ available: true, roots: ['C:\\a'] }, ['C:\\a']), {
		kind: 'available',
		roots: ['C:\\a'],
		canRebuildHere: true
	});
	assert.equal(koReindexNotice({ available: false, roots: ['C:\\a'] }, ['C:\\a'])?.kind, 'missing');
});

test('koReindexNotice offers in-app rebuild only for a single registered folder', () => {
	const notice = koReindexNotice({ available: false, roots: ['C:\\b'] }, ['C:\\a', 'C:\\b']);
	assert.equal(notice?.canRebuildHere, false);
	assert.deepEqual(notice?.roots, ['C:\\b']);
});
