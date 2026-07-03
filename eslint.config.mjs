// crates/yozist-api/assets/ 配下の静的 JS 用 ESLint flat config（issue #53 ステージ4）。
// リポジトリに Node 依存（package.json）は持ち込まない方針のため、
// `npx -y eslint crates/yozist-api/assets` で実行する（globals パッケージにも依存しない）。
// まずは未定義参照（no-undef）と未使用変数（no-unused-vars）だけを検出する。

// ブラウザ実行環境のグローバル（classic script）。ES 組み込みは languageOptions.ecmaVersion で解決される。
const browserGlobals = Object.fromEntries([
  'window', 'document', 'location', 'history', 'navigator', 'localStorage', 'sessionStorage',
  'fetch', 'Headers', 'Request', 'Response', 'AbortController',
  'URL', 'URLSearchParams', 'Blob', 'File', 'FormData',
  'TextDecoder', 'TextEncoder', 'Image', 'Audio',
  'setTimeout', 'clearTimeout', 'setInterval', 'clearInterval',
  'requestAnimationFrame', 'cancelAnimationFrame',
  'console', 'alert', 'confirm', 'prompt', 'CustomEvent', 'Event',
  'HTMLElement', 'Element', 'Node', 'NodeList',
].map(name => [name, 'readonly']));

// base.html が読み込む common.js（/ui/assets/common.js）が定義する共有グローバル。
// ページ JS・ビュープラグインはこれらを前提に動く。
const commonJsGlobals = Object.fromEntries([
  '$', 'token', 'api', 'json', 'escapeHtml', 'decodeBytes', 'fmtTs',
  'redirectToLogin', 'logout', 'requireAuth',
  'uiToast', 'uiConfirm', 'uiPrompt', 'uiCopyUrl',
  'fmtSize', 'bytesEqual', 'sniffImageMime', 'bytesLookBinary', 'looksBinaryText',
  'lcsDiffKeyed', 'diffKeyed', 'imgInfoCache', 'loadImageMeta', 'imageInfo',
  'EXT_MIME', 'TEXT_EXT', 'extOf', 'mediaKind', 'viewerKind', 'ViewRuntime',
].map(name => [name, 'readonly']));

export default [
  {
    files: ['crates/yozist-api/assets/**/*.js'],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: 'script',
      globals: { ...browserGlobals, ...commonJsGlobals },
    },
    rules: {
      'no-undef': 'error',
      // catch (e) {} で握りつぶす箇所が多いため caught error は対象外にする。
      // 関数引数はインターフェース（プラグイン mount(cont, ctx) 等）の形を保つため対象外。
      'no-unused-vars': ['error', { args: 'none', caughtErrors: 'none' }],
    },
  },
  {
    // common.js 自身は上記グローバルの定義元。トップレベル定義は他ファイルから
    // 参照されるため、未使用検出（no-unused-vars）の対象にしない。
    files: ['crates/yozist-api/assets/common.js'],
    languageOptions: { globals: { ...browserGlobals } },
    rules: { 'no-unused-vars': 'off' },
  },
];
