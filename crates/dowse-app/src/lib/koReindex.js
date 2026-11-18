/** 한국어 사전(ko-dic) 재색인 안내 판단 (순수 JS, 단위 테스트는 tests/koReindex.test.mjs). */

/**
 * 안내를 띄울지와 어떻게 띄울지 정한다. 다시 색인할 폴더가 없으면 `null`.
 * - kind: 지금 사전이 있으면 'available'(사전이 새로 생김), 없으면 'missing'(사전이 없어짐)
 * - canRebuildHere: 등록된 폴더가 하나뿐이라 화면에서 바로 전체 재색인할 수 있는지
 *   (여러 폴더면 트레이 메뉴에서 폴더별로 다시 만들어야 한다)
 * @param {{ available: boolean, roots: string[] } | null | undefined} hint
 * @param {string[]} roots 등록된 전체 색인 폴더
 * @returns {{ kind: 'available' | 'missing', roots: string[], canRebuildHere: boolean } | null}
 */
export function koReindexNotice(hint, roots) {
	if (!hint || !Array.isArray(hint.roots) || hint.roots.length === 0) return null;
	return {
		kind: hint.available ? 'available' : 'missing',
		roots: hint.roots,
		canRebuildHere: Array.isArray(roots) && roots.length === 1
	};
}
