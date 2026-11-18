import test from 'node:test';
import assert from 'node:assert/strict';

import { docPropItems, docPropsLine, formatDocDate } from '../src/lib/docProps.js';

const labels = { propTitle: '제목', propAuthor: '작성자', propCreated: '작성일', propModified: '수정일' };

function localMs(y, m, d) {
	return new Date(y, m - 1, d, 12, 0, 0).getTime();
}

test('formatDocDate renders local YYYY-MM-DD and rejects missing values', () => {
	assert.equal(formatDocDate(localMs(2024, 3, 5)), '2024-03-05');
	assert.equal(formatDocDate(null), '');
	assert.equal(formatDocDate(undefined), '');
	assert.equal(formatDocDate(Number.NaN), '');
});

test('docPropItems lists only present properties in fixed order', () => {
	const items = docPropItems(
		{ title: ' 배터리 보고서 ', author: '홍길동', doc_created: localMs(2024, 1, 2), doc_modified: null },
		labels
	);
	assert.deepEqual(items, [
		{ key: 'title', label: '제목', value: '배터리 보고서' },
		{ key: 'author', label: '작성자', value: '홍길동' },
		{ key: 'created', label: '작성일', value: '2024-01-02' }
	]);
	assert.deepEqual(docPropItems({ title: '', author: null, doc_created: null, doc_modified: null }, labels), []);
});

test('docPropsLine prefers modified date and is empty without properties', () => {
	const hit = { title: 'Plan', author: 'Kim', doc_created: localMs(2024, 1, 2), doc_modified: localMs(2024, 6, 7) };
	assert.equal(docPropsLine(hit), 'Plan · Kim · 2024-06-07');
	assert.equal(docPropsLine({ ...hit, doc_modified: null }), 'Plan · Kim · 2024-01-02');
	assert.equal(docPropsLine({ title: null, author: null, doc_created: null, doc_modified: null }), '');
});
