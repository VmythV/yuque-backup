#!/usr/bin/env node

import fs from "node:fs/promises";
import path from "node:path";
import process from "node:process";

const KEYWORD_RE =
  /lakemind|mindmap|mind-map|思维导图|脑图|lakeboard|_lake_card/i;
const DEFAULT_HOST = "https://yuque.com";

const args = parseArgs(process.argv.slice(2));

if (args.help || (!args.url && !args.scanRoot && !args.reanalyzeReport)) {
  printHelp();
  process.exit(args.help ? 0 : 2);
}

const outDir = args.outDir || "backup/.state/mindmap-probe";
const cookieHeader = buildCookieHeader(
  process.env[args.cookieEnv || "YUQUE_COOKIE"] || "",
  args.cookieName || "_yuque_session",
);
const delayMs = Number(args.delayMs || 1500);
const timeoutMs = Number(args.timeoutMs || 60_000);
const saveRaw = args.raw !== "false" && !args.noRaw;

if (!args.scanOnly && !args.reanalyzeReport && !cookieHeader) {
  throw new Error(
    `缺少 Cookie。请先设置 ${args.cookieEnv || "YUQUE_COOKIE"}，例如：export ${args.cookieEnv || "YUQUE_COOKIE"}='_yuque_session=...'`,
  );
}

await fs.mkdir(outDir, { recursive: true });

if (args.reanalyzeReport) {
  const report = await reanalyzeReport(args.reanalyzeReport);
  const reportPath =
    args.out ||
    path.join(
      path.dirname(args.reanalyzeReport),
      "mindmap-request-probe.reanalyzed.report.json",
    );
  await writeJson(reportPath, report);
  console.log(JSON.stringify(summarizeReport(report, reportPath), null, 2));
  process.exit(0);
}

const candidates = args.url
  ? [candidateFromUrl(args.url, args)]
  : await scanCandidates(args.scanRoot, args);

if (args.scanOnly) {
  const report = {
    generatedAt: new Date().toISOString(),
    mode: "scan-only",
    candidates,
  };
  const reportPath = path.join(outDir, "mindmap-candidates.json");
  await writeJson(reportPath, report);
  console.log(
    JSON.stringify(
      { candidates: candidates.length, report: reportPath },
      null,
      2,
    ),
  );
  process.exit(0);
}

const limit = args.url
  ? candidates.length
  : normalizeLimit(args.limit, 3, candidates.length);
const selected = candidates.slice(0, limit);
const results = [];

for (let index = 0; index < selected.length; index += 1) {
  const candidate = selected[index];
  const result = await probeCandidate(candidate, {
    index,
    outDir,
    cookieHeader,
    timeoutMs,
    aggressive: Boolean(args.aggressive),
    saveRaw,
  });
  results.push(result);
  if (index + 1 < selected.length) {
    await sleep(delayMs);
  }
}

const report = {
  generatedAt: new Date().toISOString(),
  mode: "request-probe",
  host: args.host || DEFAULT_HOST,
  totalCandidates: candidates.length,
  probedCandidates: selected.length,
  aggressive: Boolean(args.aggressive),
  saveRaw,
  results,
};
const reportPath = path.join(outDir, "mindmap-request-probe.report.json");
await writeJson(reportPath, report);

console.log(
  JSON.stringify(
    {
      totalCandidates: candidates.length,
      probedCandidates: selected.length,
      report: reportPath,
      rawDir: saveRaw ? path.join(outDir, "raw") : null,
    },
    null,
    2,
  ),
);

async function probeCandidate(candidate, options) {
  const host = normalizeHost(candidate.host || args.host || DEFAULT_HOST);
  const rawDir = path.join(options.outDir, "raw");
  if (options.saveRaw) {
    await fs.mkdir(rawDir, { recursive: true });
  }

  let resolved = { ...candidate, host };
  const variants = [];

  if (resolved.namespace && resolved.slug) {
    variants.push({
      name: "page-html",
      url: `${host}/${resolved.namespace}/${resolved.slug}`,
      kind: "html",
    });
  }

  const pageResults = [];
  for (const variant of variants) {
    const response = await fetchVariant(variant, options);
    const stored = options.saveRaw
      ? await saveRawResponse(
          rawDir,
          options.index,
          resolved,
          variant,
          response,
        )
      : null;
    const analysis = analyzeResponse(response);
    pageResults.push(toVariantReport(variant, response, analysis, stored));
    const appData = analysis.appData;
    if (appData) {
      resolved = {
        ...resolved,
        bookId:
          resolved.bookId ||
          valueString(appData.book, "id") ||
          valueString(appData.doc, "book_id"),
        docId: resolved.docId || valueString(appData.doc, "id"),
        slug: resolved.slug || valueString(appData.doc, "slug"),
        namespace: resolved.namespace || valueString(appData.book, "namespace"),
      };
    }
  }

  const apiResults = [];
  const apiVariants = buildApiVariants(resolved, options.aggressive);
  for (const variant of apiVariants) {
    const response = await fetchVariant(variant, options);
    const stored = options.saveRaw
      ? await saveRawResponse(
          rawDir,
          options.index,
          resolved,
          variant,
          response,
        )
      : null;
    apiResults.push(
      toVariantReport(variant, response, analyzeResponse(response), stored),
    );
  }

  return {
    candidate: sanitizeCandidate(resolved),
    variants: [...pageResults, ...apiResults],
    conclusion: conclude([...pageResults, ...apiResults]),
  };
}

async function reanalyzeReport(reportFile) {
  const reportDir = path.dirname(path.resolve(reportFile));
  const report = await readJson(reportFile);
  if (!report || !Array.isArray(report.results)) {
    throw new Error(`不是有效探测报告：${reportFile}`);
  }
  const updated = structuredClone(report);
  updated.reanalyzedAt = new Date().toISOString();
  for (const result of updated.results) {
    for (const variant of result.variants || []) {
      if (!variant.rawPath) continue;
      const rawPath = await resolveExistingPath(variant.rawPath, reportDir);
      if (!rawPath) {
        variant.analysis = {
          ...(variant.analysis || {}),
          error: `rawPath 不存在：${variant.rawPath}`,
        };
        continue;
      }
      const text = await fs.readFile(rawPath, "utf8");
      const response = {
        status: variant.status,
        ok: variant.ok,
        contentType:
          variant.contentType ||
          (looksJson(text) ? "application/json" : "text/plain"),
        bytes: Buffer.byteLength(text),
        text,
      };
      const analysis = analyzeResponse(response);
      variant.bytes = response.bytes;
      variant.analysis = toVariantReport(
        { name: variant.name, url: variant.url },
        response,
        analysis,
        variant.rawPath,
      ).analysis;
    }
    result.conclusion = conclude(result.variants || []);
  }
  return updated;
}

async function resolveExistingPath(rawPath, reportDir) {
  const candidates = path.isAbsolute(rawPath)
    ? [rawPath]
    : [
        path.resolve(rawPath),
        path.resolve(reportDir, rawPath),
        path.resolve(reportDir, "..", rawPath),
      ];
  for (const candidate of candidates) {
    try {
      await fs.access(candidate);
      return candidate;
    } catch {
      // try next path
    }
  }
  return null;
}

function summarizeReport(report, reportPath) {
  let recoverable = 0;
  let notRecoverable = 0;
  const usefulSignals = new Map();
  const observedSignals = new Map();
  for (const result of report.results || []) {
    if (result.conclusion?.requestCanRecoverStructuredData) {
      recoverable += 1;
    } else {
      notRecoverable += 1;
    }
    for (const signal of result.conclusion?.usefulSignals || []) {
      usefulSignals.set(signal, (usefulSignals.get(signal) || 0) + 1);
    }
    for (const signal of result.conclusion?.observedSignals || []) {
      observedSignals.set(signal, (observedSignals.get(signal) || 0) + 1);
    }
  }
  return {
    totalCandidates: report.totalCandidates,
    probedCandidates: report.probedCandidates,
    recoverable,
    notRecoverable,
    usefulSignals: Object.fromEntries(topEntries(usefulSignals, 100)),
    observedSignals: Object.fromEntries(topEntries(observedSignals, 100)),
    report: reportPath,
  };
}

function buildApiVariants(candidate, aggressive) {
  if (!candidate.slug) return [];
  const host = normalizeHost(candidate.host || args.host || DEFAULT_HOST);
  const base = `${host}/api/docs/${encodeURIComponent(candidate.slug)}`;
  const baseParams = {};
  if (candidate.bookId) baseParams.book_id = candidate.bookId;

  const variants = [
    {
      name: "api-markdown-static",
      params: { ...baseParams, merge_dynamic_data: "false", mode: "markdown" },
    },
    {
      name: "api-default-static",
      params: { ...baseParams, merge_dynamic_data: "false" },
    },
    {
      name: "api-lake-static",
      params: { ...baseParams, merge_dynamic_data: "false", mode: "lake" },
    },
    {
      name: "api-html-static",
      params: { ...baseParams, merge_dynamic_data: "false", mode: "html" },
    },
  ];

  if (aggressive) {
    variants.push(
      {
        name: "api-default-dynamic",
        params: { ...baseParams, merge_dynamic_data: "true" },
      },
      {
        name: "api-lake-dynamic",
        params: { ...baseParams, merge_dynamic_data: "true", mode: "lake" },
      },
      {
        name: "api-markdown-dynamic",
        params: { ...baseParams, merge_dynamic_data: "true", mode: "markdown" },
      },
      {
        name: "api-rich-markdown-static",
        params: {
          ...baseParams,
          merge_dynamic_data: "false",
          mode: "markdown",
          plain: "false",
          linebreak: "true",
          anchor: "true",
        },
      },
    );
  }

  return variants.map((variant) => ({
    name: variant.name,
    kind: "json",
    url: `${base}?${new URLSearchParams(variant.params).toString()}`,
  }));
}

async function fetchVariant(variant, options) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), options.timeoutMs);
  try {
    const response = await fetch(variant.url, {
      method: "GET",
      redirect: "follow",
      signal: controller.signal,
      headers: {
        accept:
          variant.kind === "html"
            ? "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
            : "application/json,text/plain,*/*",
        cookie: options.cookieHeader,
        "user-agent": "yuque-backup-request-probe/0.1",
      },
    });
    const bytes = Buffer.from(await response.arrayBuffer());
    return {
      status: response.status,
      ok: response.ok,
      contentType: response.headers.get("content-type") || "",
      bytes: bytes.length,
      text: bytes.toString("utf8"),
    };
  } catch (error) {
    return {
      status: 0,
      ok: false,
      contentType: "",
      bytes: 0,
      text: "",
      error: error instanceof Error ? error.message : String(error),
    };
  } finally {
    clearTimeout(timeout);
  }
}

async function saveRawResponse(rawDir, index, candidate, variant, response) {
  const ext =
    response.contentType.includes("json") || looksJson(response.text)
      ? "json"
      : "txt";
  const filename = `${String(index + 1).padStart(3, "0")}-${safeName(candidate.slug || candidate.docId || "doc")}-${safeName(variant.name)}.${ext}`;
  const target = path.join(rawDir, filename);
  await fs.writeFile(target, response.text);
  return target;
}

function analyzeResponse(response) {
  const analysis = {
    error: response.error || null,
    keywords: keywordCounts(response.text),
    cardTags: summarizeCardTags(response.text),
    lakeCardParams: summarizeLakeCardParams(response.text),
    json: null,
    appData: null,
    appDataSummary: null,
    interestingKeyPaths: [],
  };

  const json = parseJson(response.text);
  if (json) {
    analysis.json = summarizeJson(json);
    const embeddedText = extractEmbeddedTextSources(json).join("\n");
    if (embeddedText) {
      analysis.cardTags = summarizeCardTags(embeddedText);
      analysis.lakeCardParams = summarizeLakeCardParams(embeddedText);
    }
    analysis.interestingKeyPaths = collectInterestingKeyPaths(json);
    return analysis;
  }

  const appData = parseAppData(response.text);
  if (appData) {
    analysis.appData = appData;
    analysis.appDataSummary = summarizeAppData(appData);
    analysis.interestingKeyPaths = collectInterestingKeyPaths(appData);
  }
  return analysis;
}

function summarizeJson(value) {
  const data = value && typeof value === "object" ? value.data || value : {};
  const contentJson =
    typeof data.content === "string" ? parseJson(data.content) : null;
  const contentDiagramData = contentJson?.diagramData || null;
  return {
    rootKeys: objectKeys(value),
    dataKeys: objectKeys(data),
    type: valueString(data, "type"),
    format: valueString(data, "format"),
    hasSourcecode: typeof data.sourcecode === "string",
    hasBodyLake:
      typeof data.body_lake === "string" ||
      typeof data.body_draft_lake === "string",
    hasBodyHtml: typeof data.body_html === "string",
    hasContent: typeof data.content === "string",
    contentKeys: contentJson ? objectKeys(contentJson) : [],
    contentHasDiagramData: Boolean(contentDiagramData),
    contentDiagramSummary: contentDiagramData
      ? summarizeDiagramData(contentDiagramData)
      : null,
    contentInterestingKeyPaths: contentJson
      ? collectInterestingKeyPaths(contentJson)
      : [],
  };
}

function extractEmbeddedTextSources(value) {
  const data = value && typeof value === "object" ? value.data || value : {};
  return [
    data.content,
    data.content_html,
    data.sourcecode,
    data.body_lake,
    data.body_draft_lake,
    data.body_html,
  ].filter((item) => typeof item === "string" && item.length > 0);
}

function summarizeAppData(appData) {
  return {
    rootKeys: objectKeys(appData),
    docKeys: objectKeys(appData.doc),
    bookKeys: objectKeys(appData.book),
    docType: valueString(appData.doc, "type"),
    docFormat: valueString(appData.doc, "format"),
    hasDocBodyLake:
      typeof appData.doc?.body_lake === "string" ||
      typeof appData.doc?.body_draft_lake === "string",
    hasDocBodyHtml: typeof appData.doc?.body_html === "string",
    hasDocSourcecode: typeof appData.doc?.sourcecode === "string",
    bookIdPresent: Boolean(
      valueString(appData.book, "id") || valueString(appData.doc, "book_id"),
    ),
  };
}

function summarizeCardTags(text) {
  const names = new Map();
  const dataKeys = new Map();
  let count = 0;
  let parsedData = 0;
  let diagramCards = 0;
  let boardCards = 0;
  let imageCards = 0;
  const diagramSummaries = [];
  const tagRe = /<card\b[^>]*>/g;
  let match;
  while ((match = tagRe.exec(text))) {
    count += 1;
    const tag = match[0];
    const name = attrValue(tag, "name") || "unknown";
    names.set(name, (names.get(name) || 0) + 1);
    if (name === "board") boardCards += 1;
    if (name === "image") imageCards += 1;
    const value = attrValue(tag, "value");
    if (value?.startsWith("data:")) {
      const decoded = safeDecode(value.slice("data:".length));
      const json = parseJson(decoded);
      if (json && typeof json === "object") {
        parsedData += 1;
        for (const key of Object.keys(json)) {
          dataKeys.set(key, (dataKeys.get(key) || 0) + 1);
        }
        if (json.diagramData) {
          diagramCards += 1;
          diagramSummaries.push(summarizeDiagramData(json.diagramData));
        }
      }
    }
  }
  return {
    count,
    parsedData,
    diagramCards,
    boardCards,
    imageCards,
    names: topEntries(names),
    dataKeys: topEntries(dataKeys),
    diagramSummaries: diagramSummaries.slice(0, 20),
  };
}

function summarizeDiagramData(diagramData) {
  const body = diagramData?.body;
  const foldLikeKeys = new Set();
  const shapeCounts = new Map();
  let total = 0;
  let maxChildren = 0;
  let maxTreeDepth = 0;

  const visit = (node, depth) => {
    if (!node || typeof node !== "object") return;
    total += 1;
    maxTreeDepth = Math.max(maxTreeDepth, depth);
    for (const key of Object.keys(node)) {
      if (/fold|collapse|expand|visible|hide/i.test(key)) {
        foldLikeKeys.add(key);
      }
    }
    if (node.shape) {
      const shape = String(node.shape);
      shapeCounts.set(shape, (shapeCounts.get(shape) || 0) + 1);
    }
    const children = Array.isArray(node.children) ? node.children : [];
    maxChildren = Math.max(maxChildren, children.length);
    for (const child of children) {
      visit(child, depth + 1);
    }
  };

  if (Array.isArray(body)) {
    for (const item of body) {
      visit(item, 1);
    }
  }

  return {
    bodyCount: Array.isArray(body) ? body.length : null,
    totalNodes: total,
    maxChildren,
    maxTreeDepth,
    foldLikeKeys: [...foldLikeKeys].sort(),
    shapeCounts: topEntries(shapeCounts, 20),
  };
}

function mergeCardTagSummaries(summaries) {
  const names = new Map();
  const dataKeys = new Map();
  const diagramSummaries = [];
  const output = {
    count: 0,
    parsedData: 0,
    diagramCards: 0,
    boardCards: 0,
    imageCards: 0,
    names: [],
    dataKeys: [],
    diagramSummaries,
  };
  for (const summary of summaries.filter(Boolean)) {
    output.count += summary.count || 0;
    output.parsedData += summary.parsedData || 0;
    output.diagramCards += summary.diagramCards || 0;
    output.boardCards += summary.boardCards || 0;
    output.imageCards += summary.imageCards || 0;
    mergeEntries(names, summary.names);
    mergeEntries(dataKeys, summary.dataKeys);
    diagramSummaries.push(...(summary.diagramSummaries || []));
  }
  output.names = topEntries(names);
  output.dataKeys = topEntries(dataKeys);
  output.diagramSummaries = diagramSummaries.slice(0, 20);
  return output;
}

function mergeLakeCardParamSummaries(summaries) {
  const keys = new Map();
  const cardNames = new Map();
  const output = {
    count: 0,
    parsed: 0,
    keys: [],
    cardNames: [],
  };
  for (const summary of summaries.filter(Boolean)) {
    output.count += summary.count || 0;
    output.parsed += summary.parsed || 0;
    mergeEntries(keys, summary.keys);
    mergeEntries(cardNames, summary.cardNames);
  }
  output.keys = topEntries(keys);
  output.cardNames = topEntries(cardNames);
  return output;
}

function mergeEntries(target, entries) {
  for (const [key, count] of entries || []) {
    target.set(key, (target.get(key) || 0) + count);
  }
}

function summarizeLakeCardParams(text) {
  const keys = new Map();
  const cardNames = new Map();
  let count = 0;
  let parsed = 0;
  const re = /[?&#]_lake_card=([^&#\s)]+)/g;
  let match;
  while ((match = re.exec(text))) {
    count += 1;
    const json = parseJson(safeDecode(match[1]));
    if (json && typeof json === "object") {
      parsed += 1;
      for (const key of Object.keys(json)) {
        keys.set(key, (keys.get(key) || 0) + 1);
      }
      const name = json.card || json.type || json.name;
      if (name) {
        cardNames.set(String(name), (cardNames.get(String(name)) || 0) + 1);
      }
    }
  }
  return {
    count,
    parsed,
    keys: topEntries(keys),
    cardNames: topEntries(cardNames),
  };
}

function collectInterestingKeyPaths(value) {
  const output = [];
  const seen = new Set();
  const interesting =
    /mind|board|diagram|card|lake|node|edge|children|root|content|source|data|url|src/i;
  const visit = (current, prefix, depth) => {
    if (
      output.length >= 200 ||
      depth > 8 ||
      !current ||
      typeof current !== "object"
    )
      return;
    if (seen.has(current)) return;
    seen.add(current);
    if (Array.isArray(current)) {
      for (const item of current.slice(0, 5)) {
        visit(item, `${prefix}[]`, depth + 1);
      }
      return;
    }
    for (const [key, child] of Object.entries(current)) {
      const next = prefix ? `${prefix}.${key}` : key;
      if (interesting.test(key)) {
        output.push(next);
      }
      visit(child, next, depth + 1);
    }
  };
  visit(value, "", 0);
  return [...new Set(output)].slice(0, 200);
}

function keywordCounts(text) {
  const output = {};
  for (const keyword of [
    "lakemind",
    "mindmap",
    "mind-map",
    "lakeboard",
    "board",
    "diagram",
    "_lake_card",
    "body_lake",
    "body_html",
    "sourcecode",
    "content",
  ]) {
    const count = text.toLowerCase().split(keyword.toLowerCase()).length - 1;
    if (count > 0) output[keyword] = count;
  }
  return output;
}

function conclude(variantReports) {
  const useful = [];
  const observed = [];
  for (const item of variantReports) {
    const json = item.analysis.json;
    if (json?.hasBodyLake) useful.push(`${item.name}:body_lake`);
    if (json?.hasBodyHtml) useful.push(`${item.name}:body_html`);
    if (json?.contentHasDiagramData)
      useful.push(`${item.name}:content-diagramData`);
    if (json?.hasContent && json.contentKeys.length > 0)
      observed.push(`${item.name}:content-json`);
    if (item.analysis.cardTags.diagramCards > 0)
      useful.push(`${item.name}:card-diagramData`);
    if (item.analysis.cardTags.parsedData > 0)
      observed.push(`${item.name}:card-data`);
    if (item.analysis.lakeCardParams.parsed > 0)
      observed.push(`${item.name}:_lake_card`);
    if (item.analysis.appDataSummary?.hasDocBodyLake)
      useful.push(`${item.name}:appData.body_lake`);
  }
  return {
    requestCanRecoverStructuredData: useful.length > 0,
    usefulSignals: [...new Set(useful)],
    observedSignals: [...new Set(observed)],
  };
}

function toVariantReport(variant, response, analysis, rawPath) {
  return {
    name: variant.name,
    url: scrubUrl(variant.url),
    status: response.status,
    ok: response.ok,
    contentType: response.contentType,
    bytes: response.bytes,
    rawPath,
    analysis: {
      error: analysis.error,
      keywords: analysis.keywords,
      cardTags: analysis.cardTags,
      lakeCardParams: analysis.lakeCardParams,
      json: analysis.json,
      appDataSummary: analysis.appDataSummary,
      interestingKeyPaths: analysis.interestingKeyPaths,
    },
  };
}

async function scanCandidates(root, args) {
  const files = await walk(root);
  const candidates = [];
  for (const file of files.filter(
    (file) =>
      file.endsWith(".json") &&
      file.includes(`${path.sep}raw${path.sep}docs${path.sep}`),
  )) {
    const text = await fs.readFile(file, "utf8").catch(() => "");
    if (!KEYWORD_RE.test(text)) continue;
    const raw = parseJson(text);
    if (!raw) continue;
    const data = raw.data || raw;
    const repoRoot = repositoryRootFromRawDoc(file);
    const repository = await readJson(
      path.join(repoRoot, "raw", "repository.json"),
    );
    candidates.push({
      host: args.host || DEFAULT_HOST,
      namespace: valueString(repository, "namespace"),
      bookId: valueString(repository, "id"),
      slug: valueString(data, "slug"),
      docId: valueString(data, "id"),
      type: valueString(data, "type"),
      format: valueString(data, "format"),
      rawPath: file,
    });
  }
  return candidates;
}

function candidateFromUrl(input, args) {
  const url = new URL(input);
  const segments = url.pathname
    .split("/")
    .filter(Boolean)
    .map(decodeURIComponent);
  const slug = args.slug || segments.at(-1);
  const namespace = args.namespace || segments.slice(0, -1).join("/");
  return {
    host: args.host || url.origin,
    namespace,
    slug,
    bookId: args.bookId,
    docId: args.docId,
    rawPath: null,
  };
}

function repositoryRootFromRawDoc(file) {
  return path.dirname(path.dirname(path.dirname(file)));
}

async function walk(root) {
  const result = [];
  const entries = await fs
    .readdir(root, { withFileTypes: true })
    .catch(() => []);
  for (const entry of entries) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      result.push(...(await walk(full)));
    } else if (entry.isFile()) {
      result.push(full);
    }
  }
  return result;
}

async function readJson(file) {
  return parseJson(await fs.readFile(file, "utf8").catch(() => ""));
}

function parseAppData(html) {
  const patterns = [
    /decodeURIComponent\("([^"]+)"\)/g,
    /decodeURIComponent\('([^']+)'\)/g,
  ];
  for (const pattern of patterns) {
    let match;
    while ((match = pattern.exec(html))) {
      const decoded = safeDecode(match[1]);
      const json = parseJson(decoded);
      if (
        json &&
        typeof json === "object" &&
        (json.doc || json.book || json.space)
      ) {
        return json;
      }
    }
  }
  return null;
}

function attrValue(tag, name) {
  const re = new RegExp(`\\b${name}=(['\"])(.*?)\\1`);
  const match = re.exec(tag);
  return match?.[2] || null;
}

function objectKeys(value) {
  return value && typeof value === "object" && !Array.isArray(value)
    ? Object.keys(value)
    : [];
}

function valueString(value, key) {
  const item = value && typeof value === "object" ? value[key] : null;
  if (item === null || item === undefined) return null;
  return String(item);
}

function topEntries(map, limit = 40) {
  return [...map.entries()].sort((a, b) => b[1] - a[1]).slice(0, limit);
}

function parseJson(text) {
  if (!text || typeof text !== "string") return null;
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function looksJson(text) {
  const trimmed = text.trim();
  return trimmed.startsWith("{") || trimmed.startsWith("[");
}

function safeDecode(value) {
  try {
    return decodeURIComponent(
      value.replace(/&quot;/g, '"').replace(/&amp;/g, "&"),
    );
  } catch {
    return value;
  }
}

function safeName(value) {
  return (
    String(value || "untitled")
      .replace(/[\\/:*?"<>|\s]+/g, "_")
      .replace(/^_+|_+$/g, "")
      .slice(0, 100) || "untitled"
  );
}

function scrubUrl(url) {
  const parsed = new URL(url);
  parsed.searchParams.delete("token");
  parsed.searchParams.delete("key");
  return parsed.toString();
}

function sanitizeCandidate(candidate) {
  return {
    host: candidate.host,
    namespace: candidate.namespace || null,
    bookId: candidate.bookId || null,
    slug: candidate.slug || null,
    docId: candidate.docId || null,
    type: candidate.type || null,
    format: candidate.format || null,
    rawPath: candidate.rawPath || null,
  };
}

function normalizeHost(host) {
  return String(host || DEFAULT_HOST).replace(/\/+$/, "");
}

function normalizeLimit(value, fallback, total) {
  if (value === undefined) return Math.min(fallback, total);
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) return Math.min(fallback, total);
  if (parsed === 0) return total;
  return Math.min(parsed, total);
}

function buildCookieHeader(value, fallbackName) {
  const trimmed = value.trim();
  if (!trimmed) return "";
  return trimmed.includes("=") ? trimmed : `${fallbackName}=${trimmed}`;
}

async function writeJson(file, value) {
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, `${JSON.stringify(value, null, 2)}\n`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function parseArgs(argv) {
  const output = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2).replace(/-([a-z])/g, (_, ch) => ch.toUpperCase());
    if (["help", "scanOnly", "aggressive", "noRaw"].includes(key)) {
      output[key] = true;
    } else {
      output[key] = argv[index + 1];
      index += 1;
    }
  }
  return output;
}

function printHelp() {
  console.log(`
语雀思维导图 / 画板请求探测器（不使用浏览器，不修改原文档）

扫描本地疑似候选，不发请求：
  node tools/probe-yuque-mindmap-data.mjs --scan-root backup --scan-only

探测单个文档 URL：
  export YUQUE_COOKIE='_yuque_session=你的值'
  node tools/probe-yuque-mindmap-data.mjs \\
    --url 'https://yuque.com/<team>/<book>/<doc>' \\
    --book-id '<book_id>' \\
    --out-dir 'backup/.state/mindmap-probe'

扫描本地候选并低速探测前 3 个：
  node tools/probe-yuque-mindmap-data.mjs \\
    --scan-root backup \\
    --host 'https://yuque.com' \\
    --limit 3

用已保存 raw/ 重新分析报告，不发请求：
  node tools/probe-yuque-mindmap-data.mjs \\
    --reanalyze-report backup/.state/mindmap-probe/mindmap-request-probe.report.json

参数：
  --host <origin>         语雀 Host，默认 ${DEFAULT_HOST}
  --book-id <id>          单 URL 模式下建议提供；不提供时会先请求页面 HTML 尝试解析
  --limit <n>             扫描模式探测数量，默认 3；0 表示全部
  --delay-ms <n>          多文档探测间隔，默认 1500
  --aggressive            额外尝试 merge_dynamic_data=true 等变体，请谨慎使用
  --no-raw                不保存原始响应，只保存结构化报告
  --reanalyze-report <p>  用已有 report/raw 重新分析，不请求语雀
  --cookie-env <name>     Cookie 环境变量，默认 YUQUE_COOKIE
  --cookie-name <name>    当环境变量只有值没有 key= 时使用，默认 _yuque_session
`);
}
