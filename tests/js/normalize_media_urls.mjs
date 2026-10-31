// 校验 renderMd 出口的图片/链接规范化：模型爱写裸文件名与盘符绝对路径，
// 这两种在浏览器里都会 404（聊天里表现为破图）。测试直接从 static/index.html
// 里抽出真实函数体来跑，避免测试与实现漂移。
import { readFileSync } from 'node:fs';

const html = readFileSync(new URL('../../static/index.html', import.meta.url), 'utf8');
const start = html.indexOf('function normalizeMediaUrls');
if (start < 0) throw new Error('normalizeMediaUrls not found in index.html');
const end = html.indexOf('function renderMd', start);
const body = html.slice(start, end);
const normalizeMediaUrls = new Function(body + '; return normalizeMediaUrls;')();

let pass = 0, fail = 0;
function check(name, input, expected) {
  const got = normalizeMediaUrls(input);
  if (got === expected) { pass++; return; }
  fail++;
  console.log('FAIL ' + name + '\n  in   ' + input + '\n  want ' + expected + '\n  got  ' + got);
}

// 1) 真实故障：裸文件名（FoxIR 日志 5 条回复的写法）
check('bare png', '<img alt="Cover" src="en-01-cover.png">',
  '<img alt="Cover" src="/workspace/output/en-01-cover.png">');
check('bare jpg in md html', '<img src="deckA2-06-detection.png">',
  '<img src="/workspace/output/deckA2-06-detection.png">');
// 2) 真实故障：盘符绝对路径（同族实现日志里的写法）
check('drive path with workspace segment',
  '<img src="C:/Users/BUWO/.x/workspace/output/esa_workqueue_1.jpg">',
  '<img src="/workspace/output/esa_workqueue_1.jpg">');
check('drive path without workspace segment',
  '<img src="D:/shots/shot.png">', '<img src="/workspace/output/shot.png">');
check('backslash drive path', '<img src="C:\\Users\\me\\workspace\\output\\a.png">',
  '<img src="/workspace/output/a.png">');
// marked 会把 markdown 里的反斜杠转义成 %5C，真实渲染管道里看到的是这种形式
// （浏览器端到端验证时抓到的漏网用例）。
check('percent-encoded backslash drive path',
  '<img src="C:%5CUsers%5Cme%5Cworkspace%5Coutput%5Ca.png">',
  '<img src="/workspace/output/a.png">');
// 3) 相对子目录：保留子路径，只补前缀
check('relative subdir kept', '<img src="shots/p3.png">',
  '<img src="/workspace/output/shots/p3.png">');
check('dot-relative', '<img src="./x.png">', '<img src="/workspace/output/x.png">');
// 4) href 同样处理
check('href html', '<a href="report.html">r</a>', '<a href="/workspace/output/report.html">r</a>');
// 5) 既有行为不得回归：host:port 的 /workspace/... 去掉 origin
check('absolute origin stripped',
  '<img src="http://127.0.0.1:7788/workspace/output/a.png">',
  '<img src="/workspace/output/a.png">');
// 6) 必须原样不动的情况
for (const keep of [
  '<img src="/workspace/output/a.png">',
  '<img src="http://example.com/x.png">',
  '<img src="https://example.com/x.png">',
  '<img src="data:image/png;base64,iVBORw0KGgo=">',
  '<img src="/static/app.js">',
  '<a href="#top">t</a>',
  '<a href="mailto:a@b.c">m</a>',
  '<a href="https://qoder.com">q</a>',
  '<img src="not-an-image-file">']) {
  check('untouched: ' + keep.slice(9, 34), keep, keep);
}

console.log('\npass=' + pass + ' fail=' + fail);
process.exit(fail ? 1 : 0);
