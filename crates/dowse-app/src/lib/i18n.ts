// 界面中/英/韩三语字典。根布局先读取 config.lang，再挂载页面；设置修改仍在重启后生效。
// localStorage 仅作为读取配置失败时的启动兜底，不覆盖后端的权威设置。
import type { LangOption } from './types';

export const LANG_OVERRIDE_KEY = 'dowse.lang-override';

type UiLang = 'zh' | 'en' | 'ko';

// 'auto'（或未知取值）跟随系统语言：zh* → 中文，ko* → 한국어，其余英文。
function resolveUiLang(pref: string | null): UiLang {
	if (pref === 'zh' || pref === 'en' || pref === 'ko') return pref;
	const sys = navigator.language.toLowerCase();
	if (sys.startsWith('zh')) return 'zh';
	if (sys.startsWith('ko')) return 'ko';
	return 'en';
}

function readOverride(): string | null {
	try {
		return localStorage.getItem(LANG_OVERRIDE_KEY);
	} catch {
		// localStorage 不可用（隐私模式等）就当没有覆盖，回落系统语言。
		return null;
	}
}

let uiLang: UiLang = resolveUiLang(readOverride());
export let isZh = uiLang === 'zh';

interface Strings {
	// 类型筛选下拉
	filterAll: string;
	filterDoc: string;
	filterCode: string;
	filterImage: string;
	filterIdleLabel: string;
	// 排序下拉
	sortRelevance: string;
	sortNewest: string;
	sortOldest: string;
	sortLargest: string;
	sortIdleLabel: string;
	// 搜索框
	searchPlaceholder: string;
	// 结果计数分成前后缀，中间夹一个带滚动动画的数字组件
	resultsPrefix: string;
	resultsSuffix: string;
	previousPage: string;
	nextPage: string;
	pageStatus: (page: number, total: number) => string;
	// 目录选择对话框标题
	dialogPickIndexFolder: string;
	dialogAddFolder: string;
	// toast
	toastOpenFailed: (err: string) => string;
	toastRevealFailed: (err: string) => string;
	toastPathCopied: string;
	toastCopyFailed: string;
	toastRebuildDone: (n: number) => string;
	toastRebuildFailed: (err: string) => string;
	toastFolderRemoved: (n: number) => string;
	// OCR 进度条
	ocrProgressLabel: string;
	// 预览区
	previewSelectHint: string;
	ocrLoading: string;
	ocrCaption: string;
	imageViewer: string;
	closeImageViewer: string;
	imageFit: string;
	imageFitWidth: string;
	imageZoomIn: string;
	imageZoomOut: string;
	imageLoadFailed: string;
	imageEnlarge: string;

	ocrEmpty: string;
	previewLoading: string;
	previewEmpty: string;
	resizePreviewPane: string;
	// 结果列表 aria
	resultListLabel: string;
	// 图钉
	pinUnpin: string;
	pin: string;
	// 底部快捷键条
	scHide: string;
	scCopyPath: string;
	scReveal: string;
	scOpen: string;
	// 空态
	esTypeToSearch: string;
	esSearchHelp: string;
	esAddFolder: string;
	esNoIndexTitle: string;
	esNoIndexSub: string;
	esPickAndIndex: string;
	esCountUnit: string;
	esErrorTitle: string;
	esUnknownError: string;
	esRepick: string;
	esNoMatch: (numDocs: number) => string;
	esNoMatchSub: string;
	// 建索引报告里"因体积超限跳过"的补充说明（有才展示）
	esSkippedOversize: (n: number) => string;
	// 搜索历史（空态区域上方，输入框为空时展示）
	historyTitle: string;
	historyClear: string;
	historyLabel: string;
	// 建索引完成冷报告 + 秒数格式
	formatSeconds: (seconds: number) => string;
	indexReport: (indexed: string, seconds: string) => string;
	// 快捷键速查浮层
	soCardTitle: string;
	soScrimLabel: string;
	soDialogLabel: string;
	soShow: string;
	soHide: string;
	soNavigate: string;
	soOpen: string;
	soRevealFolder: string;
	soCopyPath: string;
	soFilterType: string;
	soSort: string;
	soPin: string;
	soCheatSheet: string;
	soSettings: string;
	// 设置面板（Ctrl+, 打开）：分区标题 + 通用区各项
	setTitle: string;
	setCloseLabel: string;
	setTabGeneral: string;
	setTabRules: string;
	// 通用区 - 呼出快捷键改键
	setHotkeyLabel: string;
	setHotkeyHint: string;
	setHotkeyChange: string;
	setHotkeyCapturing: string;
	setHotkeyConfirm: string;
	setHotkeyCancel: string;
	setHotkeyNeedModifier: string;
	setHotkeySaved: string;
	setHotkeyFailed: (err: string) => string;
	// 通用区 - 开机自启
	setAutostartLabel: string;
	// 通用区 - 界面语言
	setLangLabel: string;
	setLangAuto: string;
	setLangZh: string;
	setLangEn: string;
	setLangKo: string;
	setLangRestartHint: string;
	// 通用区 - 开/关两态（开机自启）
	setOn: string;
	setOff: string;
	// 索引规则面板（Ctrl+, 打开）
	rpTitle: string;
	rpCloseLabel: string;
	rpExcludeDirsLabel: string;
	rpExcludeDirsHint: string;
	rpExtraExtsLabel: string;
	rpExtraExtsHint: string;
	rpMaxFileMbLabel: string;
	rpSave: string;
	rpSaving: string;
	rpSaved: string;
	rpSaveFailed: (err: string) => string;
	rpLoadFailed: string;
	rpRebuildNow: string;
	rpRebuildMultiRootHint: string;
	rpRebuildNoIndexHint: string;
	propTitle: string;
	propAuthor: string;
	propCreated: string;
	propModified: string;
}

const zh: Strings = {
	filterAll: '全部',
	filterDoc: '文档',
	filterCode: '代码',
	filterImage: '图片',
	filterIdleLabel: '全部类型',
	sortRelevance: '相关性',
	sortNewest: '最新优先',
	sortOldest: '最旧优先',
	sortLargest: '最大优先',
	sortIdleLabel: '相关性',
	searchPlaceholder: '搜文件名或内容…',
	resultsPrefix: '结果 · ',
	resultsSuffix: ' 条',
	previousPage: '上一页',
	nextPage: '下一页',
	pageStatus: (page, total) => `第 ${page} 页，共 ${total} 页`,
	dialogPickIndexFolder: '选择要索引的目录',
	dialogAddFolder: '选择要添加的文件夹',
	toastOpenFailed: (err) => `文件打开失败：${err}`,
	toastRevealFailed: (err) => `定位文件夹失败：${err}`,
	toastPathCopied: '路径已复制。',
	toastCopyFailed: '复制失败。',
	toastRebuildDone: (n) => `索引重建完成，收录 ${n} 个文件。`,
	toastRebuildFailed: (err) => `索引重建失败：${err}`,
	toastFolderRemoved: (n) => `已移除该文件夹，删除 ${n} 篇文档。`,
	ocrProgressLabel: '图片识别',
	previewSelectHint: '选中结果后在此查看预览。',
	ocrLoading: '识别文字加载中…',
	ocrCaption: '图中文字（OCR 识别）',
	imageViewer: '图片查看',
	closeImageViewer: '返回预览',
	imageFit: '适应窗口',
	imageFitWidth: '适应宽度',
	imageZoomIn: '放大',
	imageZoomOut: '缩小',
	imageLoadFailed: '图片加载失败',
	imageEnlarge: '放大查看',

	ocrEmpty: '没有识别到文字，或者还在后台排队处理。',
	previewLoading: '加载中…',
	previewEmpty: '没有可预览的文本内容。',
	resizePreviewPane: '调整结果与预览区域宽度',
	resultListLabel: '搜索结果',
	pinUnpin: '取消固定（恢复失焦自动隐藏）',
	pin: '固定（失焦不再自动隐藏）',
	scHide: '隐藏',
	scCopyPath: '复制路径',
	scReveal: '打开所在文件夹',
	scOpen: '打开',
	esTypeToSearch: '键入即搜。',
	esSearchHelp: '文件名、文档正文都能搜，多个词默认取交集，"引号内"作短语查询。',
	esAddFolder: '添加文件夹',
	esNoIndexTitle: '尚未建立索引。',
	esNoIndexSub: '选择一个目录开始建索引，之后可在托盘菜单重建。',
	esPickAndIndex: '选择目录并建索引',
	esCountUnit: '篇',
	esErrorTitle: '索引操作失败。',
	esUnknownError: '未知错误。',
	esRepick: '重新选择目录',
	esNoMatch: (numDocs) => `没有匹配的结果。索引包含 ${numDocs} 篇文档。`,
	esNoMatchSub: '换一个查询词，或确认文件在已建索引的目录中。',
	esSkippedOversize: (n) => `其中 ${n} 个因体积超限跳过。`,
	historyTitle: '最近搜索',
	historyClear: '清空',
	historyLabel: '最近搜索列表',
	formatSeconds: (seconds) => (seconds < 10 ? `${seconds.toFixed(1)} 秒` : `${Math.round(seconds)} 秒`),
	indexReport: (indexed, seconds) => `${indexed} 篇，${seconds}。`,
	soCardTitle: '快捷键',
	soScrimLabel: '关闭快捷键速查（按任意键或点击）',
	soDialogLabel: '快捷键速查',
	soShow: '呼出',
	soHide: '隐藏',
	soNavigate: '导航',
	soOpen: '打开',
	soRevealFolder: '跳转文件夹',
	soCopyPath: '复制路径',
	soFilterType: '筛选类型',
	soSort: '排序',
	soPin: '固定',
	soCheatSheet: '速查',
	soSettings: '设置',
	setTitle: '设置',
	setCloseLabel: '关闭设置面板',
	setTabGeneral: '通用',
	setTabRules: '索引规则',
	setHotkeyLabel: '呼出快捷键',
	setHotkeyHint: '需包含至少一个修饰键（Ctrl / Alt / Shift / Win）加一个主键。',
	setHotkeyChange: '改键',
	setHotkeyCapturing: '按下新的组合键…（Esc 取消）',
	setHotkeyConfirm: '确认',
	setHotkeyCancel: '取消',
	setHotkeyNeedModifier: '至少需要一个修饰键（Ctrl / Alt / Shift / Win）。',
	setHotkeySaved: '快捷键已更新。',
	setHotkeyFailed: (err) => `改键失败：${err}`,
	setAutostartLabel: '开机自启',
	setLangLabel: '界面语言',
	setLangAuto: '跟随系统',
	setLangZh: '中文',
	setLangEn: 'English',
	setLangKo: '한국어',
	setLangRestartHint: '重启后生效。',
	setOn: '开',
	setOff: '关',
	rpTitle: '索引规则',
	rpCloseLabel: '关闭索引规则面板',
	rpExcludeDirsLabel: '排除目录',
	rpExcludeDirsHint: '目录名，逗号或换行分隔（如 node_modules, dist）。以 . 开头的目录始终排除，不用列出来。',
	rpExtraExtsLabel: '追加文本扩展名',
	rpExtraExtsHint: '不含点，逗号分隔（如 rst, adoc）。在内建白名单之外追加，不是覆盖。',
	rpMaxFileMbLabel: '单文件体积上限（MB）',
	rpSave: '保存',
	rpSaving: '保存中…',
	rpSaved: '已保存，重建索引后完全生效。',
	rpSaveFailed: (err) => `保存失败：${err}`,
	rpLoadFailed: '规则加载失败，已显示默认值。',
	rpRebuildNow: '立即重建',
	rpRebuildMultiRootHint: '已注册多个索引目录，请到托盘菜单里逐个重建。',
	rpRebuildNoIndexHint: '还没有索引目录，先在空态或托盘菜单里添加一个文件夹。',
	propTitle: '标题',
	propAuthor: '作者',
	propCreated: '创建日期',
	propModified: '修改日期'
};

const en: Strings = {
	filterAll: 'All',
	filterDoc: 'Docs',
	filterCode: 'Code',
	filterImage: 'Images',
	filterIdleLabel: 'All types',
	sortRelevance: 'Relevance',
	sortNewest: 'Newest',
	sortOldest: 'Oldest',
	sortLargest: 'Largest',
	sortIdleLabel: 'Relevance',
	searchPlaceholder: 'Search names or contents…',
	resultsPrefix: 'Results · ',
	resultsSuffix: '',
	previousPage: 'Previous page',
	nextPage: 'Next page',
	pageStatus: (page, total) => `Page ${page} of ${total}`,
	dialogPickIndexFolder: 'Choose a folder to index',
	dialogAddFolder: 'Choose a folder to add',
	toastOpenFailed: (err) => `Could not open file: ${err}`,
	toastRevealFailed: (err) => `Could not reveal folder: ${err}`,
	toastPathCopied: 'Path copied.',
	toastCopyFailed: 'Copy failed.',
	toastRebuildDone: (n) => `Index rebuilt. ${n} files indexed.`,
	toastRebuildFailed: (err) => `Index rebuild failed: ${err}`,
	toastFolderRemoved: (n) => `Folder removed. ${n} documents dropped.`,
	ocrProgressLabel: 'Image OCR',
	previewSelectHint: 'Select a result to preview it here.',
	ocrLoading: 'Loading recognized text…',
	ocrCaption: 'Text in image (OCR)',
	imageViewer: 'Image viewer',
	closeImageViewer: 'Back to preview',
	imageFit: 'Fit window',
	imageFitWidth: 'Fit width',
	imageZoomIn: 'Zoom in',
	imageZoomOut: 'Zoom out',
	imageLoadFailed: 'Could not load image',
	imageEnlarge: 'Enlarge image',

	ocrEmpty: 'No text recognized, or still queued in the background.',
	previewLoading: 'Loading…',
	previewEmpty: 'No previewable text content.',
	resizePreviewPane: 'Resize results and preview panes',
	resultListLabel: 'Search results',
	pinUnpin: 'Unpin (resume auto-hide on blur)',
	pin: 'Pin (stop auto-hide on blur)',
	scHide: 'Hide',
	scCopyPath: 'Copy path',
	scReveal: 'Reveal in File Explorer',
	scOpen: 'Open',
	esTypeToSearch: 'Type to search.',
	esSearchHelp: 'Search file names and document contents. Multiple words are ANDed; "quoted" runs a phrase query.',
	esAddFolder: 'Add folder',
	esNoIndexTitle: 'No index yet.',
	esNoIndexSub: 'Pick a folder to start indexing. You can rebuild later from the tray menu.',
	esPickAndIndex: 'Pick a folder and index',
	esCountUnit: 'docs',
	esErrorTitle: 'Indexing failed.',
	esUnknownError: 'Unknown error.',
	esRepick: 'Pick another folder',
	esNoMatch: (numDocs) => `No matches. The index holds ${numDocs} documents.`,
	esNoMatchSub: 'Try another query, or check the file is in an indexed folder.',
	esSkippedOversize: (n) => `${n} skipped for exceeding the size limit.`,
	historyTitle: 'Recent searches',
	historyClear: 'Clear',
	historyLabel: 'Recent searches list',
	formatSeconds: (seconds) => (seconds < 10 ? `${seconds.toFixed(1)} seconds` : `${Math.round(seconds)} seconds`),
	indexReport: (indexed, seconds) => `${indexed} documents, ${seconds}.`,
	soCardTitle: 'Shortcuts',
	soScrimLabel: 'Close shortcut cheat sheet (press any key or click)',
	soDialogLabel: 'Shortcut cheat sheet',
	soShow: 'Show',
	soHide: 'Hide',
	soNavigate: 'Navigate',
	soOpen: 'Open',
	soRevealFolder: 'Reveal folder',
	soCopyPath: 'Copy path',
	soFilterType: 'Filter type',
	soSort: 'Sort',
	soPin: 'Pin',
	soCheatSheet: 'Cheat sheet',
	soSettings: 'Settings',
	setTitle: 'Settings',
	setCloseLabel: 'Close settings',
	setTabGeneral: 'General',
	setTabRules: 'Index rules',
	setHotkeyLabel: 'Toggle shortcut',
	setHotkeyHint: 'Needs at least one modifier (Ctrl / Alt / Shift / Win) plus a main key.',
	setHotkeyChange: 'Change',
	setHotkeyCapturing: 'Press the new combo… (Esc to cancel)',
	setHotkeyConfirm: 'Confirm',
	setHotkeyCancel: 'Cancel',
	setHotkeyNeedModifier: 'Needs at least one modifier (Ctrl / Alt / Shift / Win).',
	setHotkeySaved: 'Shortcut updated.',
	setHotkeyFailed: (err) => `Rebind failed: ${err}`,
	setAutostartLabel: 'Launch at startup',
	setLangLabel: 'Language',
	setLangAuto: 'System',
	setLangZh: '中文',
	setLangEn: 'English',
	setLangKo: '한국어',
	setLangRestartHint: 'Takes effect after restart.',
	setOn: 'On',
	setOff: 'Off',
	rpTitle: 'Index rules',
	rpCloseLabel: 'Close index rules panel',
	rpExcludeDirsLabel: 'Excluded folders',
	rpExcludeDirsHint:
		'Folder names, comma- or newline-separated (e.g. node_modules, dist). Dot-folders are always excluded, no need to list them.',
	rpExtraExtsLabel: 'Extra text extensions',
	rpExtraExtsHint: 'No leading dot, comma-separated (e.g. rst, adoc). Adds to the built-in list, does not replace it.',
	rpMaxFileMbLabel: 'Max file size (MB)',
	rpSave: 'Save',
	rpSaving: 'Saving…',
	rpSaved: 'Saved. Rebuild the index for this to fully take effect.',
	rpSaveFailed: (err) => `Save failed: ${err}`,
	rpLoadFailed: 'Could not load rules; showing defaults.',
	rpRebuildNow: 'Rebuild now',
	rpRebuildMultiRootHint: 'Multiple index folders are registered; rebuild each one from the tray menu.',
	rpRebuildNoIndexHint: 'No index folder yet — add one from the empty state or the tray menu first.',
	propTitle: 'Title',
	propAuthor: 'Author',
	propCreated: 'Created',
	propModified: 'Modified'
};

const ko: Strings = {
	filterAll: '전체',
	filterDoc: '문서',
	filterCode: '코드',
	filterImage: '이미지',
	filterIdleLabel: '모든 형식',
	sortRelevance: '관련도순',
	sortNewest: '최신순',
	sortOldest: '오래된순',
	sortLargest: '큰 파일순',
	sortIdleLabel: '관련도순',
	searchPlaceholder: '파일 이름이나 내용 검색…',
	resultsPrefix: '결과 · ',
	resultsSuffix: '건',
	previousPage: '이전 페이지',
	nextPage: '다음 페이지',
	pageStatus: (page, total) => `${total}페이지 중 ${page}페이지`,
	dialogPickIndexFolder: '색인할 폴더 선택',
	dialogAddFolder: '추가할 폴더 선택',
	toastOpenFailed: (err) => `파일을 열지 못했습니다: ${err}`,
	toastRevealFailed: (err) => `폴더를 열지 못했습니다: ${err}`,
	toastPathCopied: '경로를 복사했습니다.',
	toastCopyFailed: '복사하지 못했습니다.',
	toastRebuildDone: (n) => `색인을 다시 만들었습니다. 파일 ${n}개를 색인했습니다.`,
	toastRebuildFailed: (err) => `색인을 다시 만들지 못했습니다: ${err}`,
	toastFolderRemoved: (n) => `폴더를 제거했습니다. 문서 ${n}개를 색인에서 뺐습니다.`,
	ocrProgressLabel: '이미지 글자 인식',
	previewSelectHint: '결과를 선택하면 여기에 미리 보기가 표시됩니다.',
	ocrLoading: '인식한 글자를 불러오는 중…',
	ocrCaption: '이미지 속 글자 (OCR)',
	imageViewer: '이미지 보기',
	closeImageViewer: '미리 보기로 돌아가기',
	imageFit: '창에 맞추기',
	imageFitWidth: '너비에 맞추기',
	imageZoomIn: '확대',
	imageZoomOut: '축소',
	imageLoadFailed: '이미지를 불러오지 못했습니다',
	imageEnlarge: '크게 보기',

	ocrEmpty: '인식된 글자가 없거나 아직 백그라운드에서 처리를 기다리는 중입니다.',
	previewLoading: '불러오는 중…',
	previewEmpty: '미리 볼 수 있는 텍스트가 없습니다.',
	resizePreviewPane: '결과 영역과 미리 보기 영역 너비 조절',
	resultListLabel: '검색 결과',
	pinUnpin: '고정 해제 (다른 창을 누르면 다시 자동으로 숨김)',
	pin: '고정 (다른 창을 눌러도 숨기지 않음)',
	scHide: '숨기기',
	scCopyPath: '경로 복사',
	scReveal: '파일 탐색기에서 보기',
	scOpen: '열기',
	esTypeToSearch: '입력하면 바로 검색합니다.',
	esSearchHelp: '파일 이름과 문서 내용을 검색합니다. 여러 단어를 입력하면 모두 포함된 결과를 찾고, "따옴표"로 묶으면 구문 그대로 검색합니다.',
	esAddFolder: '폴더 추가',
	esNoIndexTitle: '아직 색인이 없습니다.',
	esNoIndexSub: '폴더를 선택해 색인을 시작하세요. 나중에 트레이 메뉴에서 다시 만들 수 있습니다.',
	esPickAndIndex: '폴더 선택하고 색인하기',
	esCountUnit: '개 문서',
	esErrorTitle: '색인 작업에 실패했습니다.',
	esUnknownError: '알 수 없는 오류입니다.',
	esRepick: '다른 폴더 선택',
	esNoMatch: (numDocs) => `일치하는 결과가 없습니다. 색인에 문서 ${numDocs}개가 있습니다.`,
	esNoMatchSub: '다른 검색어를 입력하거나, 파일이 색인된 폴더에 있는지 확인하세요.',
	esSkippedOversize: (n) => `이 중 ${n}개는 크기 제한을 넘어 건너뛰었습니다.`,
	historyTitle: '최근 검색',
	historyClear: '지우기',
	historyLabel: '최근 검색 목록',
	formatSeconds: (seconds) => (seconds < 10 ? `${seconds.toFixed(1)}초` : `${Math.round(seconds)}초`),
	indexReport: (indexed, seconds) => `문서 ${indexed}개, ${seconds}.`,
	soCardTitle: '단축키',
	soScrimLabel: '단축키 안내 닫기 (아무 키나 누르거나 클릭)',
	soDialogLabel: '단축키 안내',
	soShow: '불러오기',
	soHide: '숨기기',
	soNavigate: '이동',
	soOpen: '열기',
	soRevealFolder: '폴더로 이동',
	soCopyPath: '경로 복사',
	soFilterType: '형식 필터',
	soSort: '정렬',
	soPin: '고정',
	soCheatSheet: '단축키 안내',
	soSettings: '설정',
	setTitle: '설정',
	setCloseLabel: '설정 닫기',
	setTabGeneral: '일반',
	setTabRules: '색인 규칙',
	setHotkeyLabel: '불러오기 단축키',
	setHotkeyHint: '보조 키(Ctrl / Alt / Shift / Win)를 하나 이상 포함하고 주 키를 하나 더해야 합니다.',
	setHotkeyChange: '변경',
	setHotkeyCapturing: '새 키 조합을 누르세요… (Esc로 취소)',
	setHotkeyConfirm: '확인',
	setHotkeyCancel: '취소',
	setHotkeyNeedModifier: '보조 키(Ctrl / Alt / Shift / Win)가 하나 이상 필요합니다.',
	setHotkeySaved: '단축키를 변경했습니다.',
	setHotkeyFailed: (err) => `단축키를 변경하지 못했습니다: ${err}`,
	setAutostartLabel: '시작 시 자동 실행',
	setLangLabel: '언어',
	setLangAuto: '시스템 설정',
	setLangZh: '中文',
	setLangEn: 'English',
	setLangKo: '한국어',
	setLangRestartHint: '다시 시작하면 적용됩니다.',
	setOn: '켬',
	setOff: '끔',
	rpTitle: '색인 규칙',
	rpCloseLabel: '색인 규칙 닫기',
	rpExcludeDirsLabel: '제외할 폴더',
	rpExcludeDirsHint:
		'폴더 이름을 쉼표나 줄바꿈으로 구분해 입력하세요 (예: node_modules, dist). 점(.)으로 시작하는 폴더는 항상 제외되므로 적지 않아도 됩니다.',
	rpExtraExtsLabel: '추가 텍스트 확장자',
	rpExtraExtsHint: '점 없이 쉼표로 구분해 입력하세요 (예: rst, adoc). 기본 목록을 대체하지 않고 더합니다.',
	rpMaxFileMbLabel: '파일 크기 상한 (MB)',
	rpSave: '저장',
	rpSaving: '저장 중…',
	rpSaved: '저장했습니다. 색인을 다시 만들어야 완전히 적용됩니다.',
	rpSaveFailed: (err) => `저장하지 못했습니다: ${err}`,
	rpLoadFailed: '규칙을 불러오지 못해 기본값을 표시합니다.',
	rpRebuildNow: '지금 다시 만들기',
	rpRebuildMultiRootHint: '색인 폴더가 여러 개 등록되어 있습니다. 트레이 메뉴에서 폴더별로 다시 만드세요.',
	rpRebuildNoIndexHint: '아직 색인 폴더가 없습니다. 빈 화면이나 트레이 메뉴에서 먼저 폴더를 추가하세요.',
	propTitle: '제목',
	propAuthor: '작성자',
	propCreated: '작성일',
	propModified: '수정일'
};

const DICTS: Record<UiLang, Strings> = { zh, en, ko };

export let t: Strings = DICTS[uiLang];

// 必须在页面组件创建前调用，避免组件初始化时捕获旧语言。
export function initializeLanguage(lang: LangOption): void {
	uiLang = resolveUiLang(lang);
	isZh = uiLang === 'zh';
	t = DICTS[uiLang];
	try {
		localStorage.setItem(LANG_OVERRIDE_KEY, lang);
	} catch {
		// 缓存不可用不影响本次启动使用 config.lang。
	}
}
