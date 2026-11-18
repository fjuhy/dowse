// 설치 파일에 넣을 한국어 형태소 사전(ko-dic)을 src-tauri/ko-dic/에 준비한다.
//
// dowse는 사전을 바이너리에 내장하지 않고 실행 파일 옆 `ko-dic/`에서 읽는다
// (crates/dowse/src/tokenizer.rs). 설치 파일에 넣을 때는 이 스크립트로 사전을
// 준비한 뒤 `tauri build --config src-tauri/tauri.ko-dic.conf.json`으로 resources를
// 추가한다. 기본 tauri.conf.json에는 resources를 걸지 않아서, 사전이 없는 개발
// 환경(맥 포함)의 빌드는 영향을 받지 않는다.
//
// 사전 찾는 순서(컴파일된 lindera 사전 디렉터리, metadata.json이 있어야 함):
//   1. DOWSE_KO_DIC 환경 변수
//   2. LINDERA_BUILD_DICTIONARY_CACHE_DIR/<버전>/lindera-ko-dic
//   3. <repo>/target/<profile>/build/lindera-ko-dic-*/out/lindera-ko-dic
//      (dowse 테스트용 dev-dependency의 build.rs가 만든 결과)
// `--build`를 주면 못 찾았을 때 `cargo check -p dowse --tests`로 위 3을 만든다
// (mecab-ko-dic 원본 약 48MB를 내려받아 컴파일).
//
// 못 찾으면 경고만 내고 0으로 끝난다(건너뜀). `--require`면 실패로 끝낸다.
// GitHub Actions에서는 GITHUB_OUTPUT에 ready=true|false를 남긴다.
//
// 사용: node scripts/prepare-ko-dic.mjs [--build] [--require]

import { spawnSync } from 'node:child_process';
import { appendFileSync, cpSync, existsSync, readdirSync, rmSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

const appDir = resolve(import.meta.dirname, '..');
const repoDir = resolve(appDir, '..', '..');
const destDir = join(appDir, 'src-tauri', 'ko-dic');
const args = new Set(process.argv.slice(2));

function isDict(dir) {
	return Boolean(dir) && existsSync(join(dir, 'metadata.json'));
}

function listDirs(dir) {
	try {
		return readdirSync(dir, { withFileTypes: true })
			.filter((e) => e.isDirectory())
			.map((e) => join(dir, e.name));
	} catch {
		return [];
	}
}

/** 후보 중 metadata.json이 가장 최근인 사전 디렉터리. */
function newest(dirs) {
	return dirs
		.filter(isDict)
		.map((dir) => ({ dir, mtime: statSync(join(dir, 'metadata.json')).mtimeMs }))
		.sort((a, b) => b.mtime - a.mtime)[0]?.dir;
}

function findDictionary() {
	const env = process.env.DOWSE_KO_DIC;
	if (isDict(env)) return env;

	const cache = process.env.LINDERA_BUILD_DICTIONARY_CACHE_DIR;
	if (cache) {
		const hit = newest(listDirs(resolve(repoDir, cache)).map((d) => join(d, 'lindera-ko-dic')));
		if (hit) return hit;
	}

	const targetDir = process.env.CARGO_TARGET_DIR
		? resolve(repoDir, process.env.CARGO_TARGET_DIR)
		: join(repoDir, 'target');
	// target/<profile>/build 와 target/<triple>/<profile>/build 둘 다 본다. build 아래는
	// 예전 배치(lindera-ko-dic-<hash>/out)와 새 배치(lindera-ko-dic/<hash>/out) 둘 다 있다.
	const profileDirs = listDirs(targetDir).flatMap((d) => [d, ...listDirs(d)]);
	const buildOuts = profileDirs
		.flatMap((d) => listDirs(join(d, 'build')))
		.flatMap((d) => {
			if (/[\\/]lindera-ko-dic-[^\\/]+$/.test(d)) return [d];
			if (/[\\/]lindera-ko-dic$/.test(d)) return listDirs(d);
			return [];
		})
		.map((d) => join(d, 'out', 'lindera-ko-dic'));
	return newest(buildOuts);
}

/** lindera-ko-dic 크레이트의 NOTICE(mecab-ko-dic 라이선스 고지) 경로. */
function findNotice() {
	const res = spawnSync('cargo', ['metadata', '--format-version', '1', '--locked'], {
		cwd: repoDir,
		encoding: 'utf8',
		maxBuffer: 256 * 1024 * 1024
	});
	if (res.status !== 0) return undefined;
	const pkg = JSON.parse(res.stdout).packages.find((p) => p.name === 'lindera-ko-dic');
	if (!pkg) return undefined;
	const notice = join(dirname(pkg.manifest_path), 'NOTICE.txt');
	return existsSync(notice) ? notice : undefined;
}

function setOutput(ready) {
	if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `ready=${ready}\n`);
}

let source = findDictionary();
if (!source && args.has('--build')) {
	console.log('ko-dic 사전을 찾지 못해 cargo check -p dowse --tests로 만듭니다…');
	const res = spawnSync('cargo', ['check', '-p', 'dowse', '--tests', '--locked'], {
		cwd: repoDir,
		stdio: 'inherit'
	});
	if (res.status === 0) source = findDictionary();
}

if (!source) {
	const msg =
		'ko-dic 사전을 찾지 못해 설치 파일에 넣지 않습니다(한글은 어절 단위로 색인됨). ' +
		'--build로 다시 실행하거나 DOWSE_KO_DIC을 지정하세요.';
	if (process.env.GITHUB_ACTIONS) console.log(`::warning::${msg}`);
	else console.warn(msg);
	setOutput(false);
	process.exit(args.has('--require') ? 1 : 0);
}

rmSync(destDir, { recursive: true, force: true });
cpSync(source, destDir, { recursive: true });
const notice = findNotice();
if (notice) cpSync(notice, join(destDir, 'NOTICE.txt'));

const files = readdirSync(destDir);
const bytes = files.reduce((sum, f) => sum + statSync(join(destDir, f)).size, 0);
console.log(
	`ko-dic 준비 완료: ${source} → ${destDir} (${files.length}개 파일, ${(bytes / 1024 / 1024).toFixed(1)}MB` +
		`${notice ? ', NOTICE 포함' : ', NOTICE 없음'})`
);
setOutput(true);
