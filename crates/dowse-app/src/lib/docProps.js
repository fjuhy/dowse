/** 문서 속성 표시 헬퍼 (순수 JS, 단위 테스트는 tests/docProps.test.mjs). */

/** @param {number} n */
function pad2(n) {
	return String(n).padStart(2, '0');
}

/**
 * UTC 밀리초 타임스탬프를 로컬 날짜 `YYYY-MM-DD`로. 값이 없거나 잘못되면 ''.
 * @param {number | null | undefined} ms
 */
export function formatDocDate(ms) {
	if (typeof ms !== 'number' || !Number.isFinite(ms)) return '';
	const d = new Date(ms);
	if (Number.isNaN(d.getTime())) return '';
	return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
}

/**
 * 표시할 문서 속성 목록. 값이 비어 있는 항목은 뺀다.
 * @param {{ author?: string | null, title?: string | null, doc_created?: number | null, doc_modified?: number | null }} hit
 * @param {{ propTitle: string, propAuthor: string, propCreated: string, propModified: string }} labels
 * @returns {{ key: 'title' | 'author' | 'created' | 'modified', label: string, value: string }[]}
 */
export function docPropItems(hit, labels) {
	/** @type {{ key: 'title' | 'author' | 'created' | 'modified', label: string, value: string }[]} */
	const items = [];
	const title = (hit.title ?? '').trim();
	const author = (hit.author ?? '').trim();
	const created = formatDocDate(hit.doc_created);
	const modified = formatDocDate(hit.doc_modified);
	if (title) items.push({ key: 'title', label: labels.propTitle, value: title });
	if (author) items.push({ key: 'author', label: labels.propAuthor, value: author });
	if (created) items.push({ key: 'created', label: labels.propCreated, value: created });
	if (modified) items.push({ key: 'modified', label: labels.propModified, value: modified });
	return items;
}

/**
 * 결과 행에 붙일 한 줄 요약: 제목 · 작성자 · 수정일(없으면 작성일). 속성이 없으면 ''.
 * @param {{ author?: string | null, title?: string | null, doc_created?: number | null, doc_modified?: number | null }} hit
 */
export function docPropsLine(hit) {
	const parts = [];
	const title = (hit.title ?? '').trim();
	const author = (hit.author ?? '').trim();
	const date = formatDocDate(hit.doc_modified) || formatDocDate(hit.doc_created);
	if (title) parts.push(title);
	if (author) parts.push(author);
	if (date) parts.push(date);
	return parts.join(' · ');
}
