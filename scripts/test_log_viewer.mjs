// Run with: node --test scripts/test_log_viewer.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const html = readFileSync(
  new URL("../assets/log_viewer.html", import.meta.url),
  "utf8",
);
const script = html.match(/<script id="viewer-script">([\s\S]*?)<\/script>/)[1];
const model = script.split("// Browser bindings.")[0];
const storage = html.match(
  /<script id="storage-script">([\s\S]*?)<\/script>/,
)[1];

function evaluate(expression) {
  const context = vm.createContext({});
  return vm.runInContext(`${storage}\n${model}\n${expression}`, context);
}

function json(expression) {
  return JSON.parse(evaluate(`JSON.stringify(${expression})`));
}

test("request ID zero and displayed string IDs identify the same request", () => {
  assert.equal(evaluate("identity({fields:{req_id:0}}).key"), "req:0");
  assert.equal(evaluate('identity({fields:{req_id:"0"}}).key'), "req:0");
  assert.equal(
    evaluate(
      'identity({spans:[{name:"inbound",fields:{req_id:3}},{name:"tcp_req",fields:{req_id:"3",dst:"example.com:443"}}]}).key',
    ),
    "req:3",
  );
});

test("log fields and span fields join the same request group", () => {
  const records = [
    { event: "socks:connection_started", fields: { req_id: 42 }, spans: [] },
    { event: "proxy:request_accepted", req_id: "42" },
    {
      event: "routing:outbound_selected",
      spans: [{ name: "tcp_req", fields: { req_id: "42" } }],
    },
  ];
  assert.deepEqual(
    json(
      `Array.from(groupRecords(${JSON.stringify(records)}.map(normalize)).values())`,
    ).map(({ key, count }) => ({ key, count })),
    [{ key: "req:42", count: 3 }],
  );
  assert.equal(evaluate("identity({req_id:0}).key"), "req:0");
  assert.equal(
    evaluate('identity({req_id:0,dst:"example.com:443"}).subtitle'),
    "example.com:443",
  );
});

test("explicit log IDs take precedence over inherited span IDs", () => {
  assert.equal(
    evaluate(
      "identity({fields:{req_id:0},req_id:99,spans:[{fields:{req_id:7}}]}).key",
    ),
    "req:0",
  );
  assert.equal(
    evaluate('identity({req_id:"12",spans:[{fields:{req_id:7}}]}).key'),
    "req:12",
  );
});

test("empty or invalid IDs fall back to the nearest usable span ID", () => {
  for (const value of [null, "", "  ", false, {}, [], -1, 1.5]) {
    assert.equal(
      evaluate(
        `identity({fields:{req_id:${JSON.stringify(value)}},spans:[{fields:{req_id:7}},{fields:{req_id:null}}]}).key`,
      ),
      "req:7",
    );
  }
  assert.equal(
    evaluate("identity({spans:[{fields:{req_id:7}},{fields:{req_id:8}}]}).key"),
    "req:8",
  );
});

test("connection IDs are scoped by inbound and source, never by destination alone", () => {
  const keys = json(
    '["one","two"].map(tag=>identity({spans:[{name:"inbound",fields:{tag,src:"127.0.0.1:4000",id:7}}]}).key)',
  );
  assert.notEqual(keys[0], keys[1]);
  assert.equal(
    evaluate(
      'identity({fields:{dst:"example.com:443"},spans:[{name:"bistream",fields:{id:7}}]}).key',
    ),
    "unscoped",
  );
});

test("timestamps preserve sub-millisecond differences and time zone offsets", () => {
  const delta = evaluate(
    'timestamp("2026-11-07T08:00:00.000200Z")-timestamp("2026-11-07T08:00:00.000100Z")',
  );
  assert.ok(Math.abs(delta - 0.1) < 0.001);
  assert.equal(
    evaluate(
      'timestamp("2026-11-07T16:00:00+08:00")-timestamp("2026-11-07T08:00:00Z")',
    ),
    0,
  );
  assert.equal(evaluate('timestamp("invalid")'), null);
});

test("imports accept JSON arrays, wrappers, single records, and JSONL", () => {
  for (const text of [
    '[{"event":"dns:cache_hit"}]',
    '{"records":[{"event":"dns:cache_hit"}]}',
    '{"event":"dns:cache_hit"}',
  ]) {
    assert.deepEqual(json(`parseCapture(${JSON.stringify(text)})`), [
      { event: "dns:cache_hit" },
    ]);
  }
  assert.equal(
    evaluate('parseCapture(\' {"event":"one"}\\n\\n{"event":"two"} \').length'),
    2,
  );
  assert.equal(evaluate('parseCapture("   ").length'), 0);
  assert.throws(() => evaluate('parseCapture("\\n{}\\n\\n{broken")'), /line 4/);
});

test("lag notices and events without timestamps remain untimed records", () => {
  assert.deepEqual(
    json(
      '((r)=>({time:r.time,event:r.event,group:r.group.key}))(normalize({event:"api:logs_lagged",dropped:7}))',
    ),
    { time: null, event: "api:logs_lagged", group: "unscoped" },
  );
  assert.equal(
    evaluate('normalize({fields:{message:"unclassified"}}).category'),
    "uncategorized",
  );
});

test("malformed structured records are rejected before capture is replaced", () => {
  for (const input of [
    "null",
    "[]",
    '{"fields":[]}',
    '{"spans":[null]}',
    '{"event":3}',
    '{"timestamp":"bad"}',
  ]) {
    assert.throws(() => evaluate(`normalize(${input})`));
  }
});

function streamed(text, size = 1) {
  return json(`(() => {
    const parser = new CaptureParser(), text = ${JSON.stringify(text)}, records = [];
    for (let i = 0; i < text.length; i += ${size}) records.push(...parser.push(text.slice(i, i + ${size})));
    records.push(...parser.push('', true));
    return records;
  })()`);
}

test("streaming imports handle arrays, wrappers and JSONL across every character boundary", () => {
  const records = [
    {
      event: "dns:query",
      fields: {
        message: 'quote " slash \\ newline\n 中文 😀',
        nested: [{ text: "[}]" }],
      },
    },
    { event: "proxy:complete", spans: [{ fields: { req_id: 0 } }] },
  ];
  for (const text of [
    JSON.stringify(records),
    JSON.stringify({ version: 1, records, note: "after" }),
    records.map((r) => JSON.stringify(r)).join("\r\n"),
  ]) {
    for (const size of [1, 2, 7, 64, 1024])
      assert.deepEqual(streamed("\uFEFF \n" + text, size), records);
  }
  assert.deepEqual(streamed(JSON.stringify(records[0])), [records[0]]);
  assert.deepEqual(streamed("[]"), []);
  assert.deepEqual(streamed('{"records":[]}'), []);
});

test("streaming imports reject incomplete records and invalid envelope separators", () => {
  for (const text of [
    '[{"event":"one"},]',
    '[{"event":"one"}',
    '{"records":[{}]',
    '{"records":[{}],}',
    "{} garbage",
    '{"records":[],"records":[]}',
  ]) {
    assert.throws(() => streamed(text), undefined, text);
  }
});

test("storage limits accept only positive safe whole numbers", () => {
  for (const value of [1, 20000, 1000000, Number.MAX_SAFE_INTEGER])
    assert.equal(evaluate(`validLimit(${value})`), true);
  for (const value of [0, -1, 1.5, null, "20000", Number.MAX_SAFE_INTEGER + 1])
    assert.equal(evaluate(`validLimit(${JSON.stringify(value)})`), false);
});
