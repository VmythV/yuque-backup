# 语雀思维导图 / 画板请求探测方案

目标：不控制浏览器、不修改语雀原文档，只通过 HTTP 请求判断是否能拿到思维导图 / 画板的完整结构化数据。

当前判断：

1. 已有 Markdown 模式请求只返回 `sourcecode`，折叠预览图无法反推出隐藏节点。
2. 如果语雀接口或页面 HTML 中返回 `body_lake`、`body_html`、`content`、`<card value="data:...">` 或 `_lake_card`，就有机会在本地保存完整数据。
3. 如果所有请求都只返回静态图片 URL，则请求层无法恢复折叠节点，只能保留原始图片和风险报告。

## 探测器

脚本位置：

```bash
tools/probe-yuque-mindmap-data.mjs
```

它不会使用 Playwright，也不会执行页面 JavaScript。它只做普通 HTTP GET：

- 页面 HTML：`/<namespace>/<slug>`
- 当前 Markdown API：`/api/docs/<slug>?book_id=...&merge_dynamic_data=false&mode=markdown`
- 默认 API：`/api/docs/<slug>?book_id=...&merge_dynamic_data=false`
- Lake API 试探：`/api/docs/<slug>?book_id=...&merge_dynamic_data=false&mode=lake`
- HTML API 试探：`/api/docs/<slug>?book_id=...&merge_dynamic_data=false&mode=html`

加 `--aggressive` 后会额外尝试 `merge_dynamic_data=true` 等变体。该选项请求数更多，先不要大批量使用。

## 设置 Cookie

```bash
export YUQUE_COOKIE='_yuque_session=你的值'
```

如果企业空间 Cookie 名不是 `_yuque_session`：

```bash
export YUQUE_COOKIE='实际Cookie名=你的值'
```

不要把 Cookie 写入命令行参数、配置文件或报告。

## 只扫描本地候选，不请求语雀

```bash
node tools/probe-yuque-mindmap-data.mjs \
  --scan-root backup \
  --scan-only
```

输出：

```text
backup/.state/mindmap-probe/mindmap-candidates.json
```

该文件只记录候选文档路径、slug、book id、文档类型等元数据，不包含正文内容。

## 探测单个文档

如果你知道知识库 ID，建议显式传入：

```bash
node tools/probe-yuque-mindmap-data.mjs \
  --url 'https://yuque.com/<team>/<book>/<doc>' \
  --book-id '<book_id>' \
  --out-dir 'backup/.state/mindmap-probe'
```

如果不知道 `book_id`，脚本会先请求页面 HTML，尝试从页面里的 appData 解析。

## 扫描候选并低速探测

默认只探测前 3 个候选，避免一次性消耗太多请求额度：

```bash
node tools/probe-yuque-mindmap-data.mjs \
  --scan-root backup \
  --host 'https://yuque.com' \
  --limit 3 \
  --delay-ms 1500
```

`--limit 0` 表示探测全部候选，不建议在未确认接口行为前使用。

## 输出文件

默认输出目录：

```text
backup/.state/mindmap-probe/
```

核心文件：

```text
mindmap-request-probe.report.json
raw/*.json
raw/*.txt
```

`report.json` 是结构化报告，重点看：

- `conclusion.requestCanRecoverStructuredData`
  - `true`：某个请求变体拿到了结构化数据，后续可以接入主同步。
  - `false`：目前请求只看到静态结果，没有发现可恢复结构。
- `conclusion.usefulSignals`
  - 例如 `api-lake-static:body_lake`、`api-default-static:content-diagramData`、`api-default-static:card-diagramData`、`page-html:appData.body_lake`。
- `conclusion.observedSignals`
  - 记录普通 `content-json`、`card-data` 等观察信号；这些信号不一定代表能恢复完整思维导图。
- `variants[].analysis.json.hasBodyLake`
- `variants[].analysis.json.hasBodyHtml`
- `variants[].analysis.json.hasContent`
- `variants[].analysis.json.contentHasDiagramData`
- `variants[].analysis.cardTags.diagramCards`
- `variants[].analysis.cardTags.parsedData`
- `variants[].analysis.lakeCardParams.parsed`

`raw/` 中保存原始响应，方便后续接入解析器。它可能包含私有文档内容，只应留在本机，不要直接发给他人。

如果只想保存结构化报告，不保存原始响应：

```bash
node tools/probe-yuque-mindmap-data.mjs \
  --url 'https://yuque.com/<team>/<book>/<doc>' \
  --book-id '<book_id>' \
  --no-raw
```

如果脚本升级后想用已有 `raw/` 重新分析，不重新请求语雀：

```bash
node tools/probe-yuque-mindmap-data.mjs \
  --reanalyze-report backup/.state/mindmap-probe/mindmap-request-probe.report.json
```

默认输出：

```text
backup/.state/mindmap-probe/mindmap-request-probe.reanalyzed.report.json
```

## 后续接入标准

探测报告满足以下任一条件，就可以进入主同步实现：

1. 某个 API 变体返回 `body_lake` 或 `body_draft_lake`。
2. 某个 API 变体返回可解析的 `content` JSON，并包含 mind / board 节点结构。
3. 页面 HTML 的 appData 包含 `doc.body_lake` 或完整 card 数据。
4. `<card value="data:...">` 或 `_lake_card` 中包含完整节点数据。

主同步已经按同一判定接入。对疑似文档会额外请求一次无 `mode` 的默认 API，并保存：

```text
diagrams/<doc-id>/request-default.raw.json
diagrams/<doc-id>/diagram.raw.json
diagrams/<doc-id>/diagram.normalized.json
diagrams/<doc-id>/diagram-report.json
```

如果上述条件都不满足，说明当前可请求数据只有静态图片。此时不能保证“全展开”，只能在归档中标记信息缺失风险。
