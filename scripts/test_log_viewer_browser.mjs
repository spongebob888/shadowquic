// Run with: CHROME_EXECUTABLE_PATH=/path/to/chrome node --test scripts/test_log_viewer_browser.mjs
// Uses Chrome's built-in DevTools protocol; no npm dependencies are required.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

class Browser {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    socket.addEventListener("message", ({ data }) => {
      const message = JSON.parse(data);
      if (!message.id) return;
      const pending = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (message.error) pending.reject(Error(JSON.stringify(message.error)));
      else pending.resolve(message.result);
    });
    socket.addEventListener("close", () => {
      for (const pending of this.pending.values())
        pending.reject(Error("Browser disconnected"));
      this.pending.clear();
    });
  }
  call(method, params = {}, sessionId) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(Error("Browser command timed out: " + method));
      }, 60000);
      timer.unref();
      this.pending.set(id, {
        resolve(value) {
          clearTimeout(timer);
          resolve(value);
        },
        reject(error) {
          clearTimeout(timer);
          reject(error);
        },
      });
      this.socket.send(
        JSON.stringify({
          id,
          method,
          params,
          ...(sessionId ? { sessionId } : {}),
        }),
      );
    });
  }
  async evaluate(session, expression) {
    const result = await this.call(
      "Runtime.evaluate",
      { expression, awaitPromise: true, returnByValue: true },
      session,
    );
    if (result.exceptionDetails)
      throw Error(
        result.exceptionDetails.exception?.description ||
          result.exceptionDetails.text,
      );
    return result.result.value;
  }
  async wait(session, expression, timeout = 30000) {
    const deadline = Date.now() + timeout;
    do {
      try {
        if (await this.evaluate(session, expression)) return;
      } catch (error) {
        if (
          !/context.*destroyed|Cannot find context|state is not defined/i.test(
            error.message,
          )
        )
          throw error;
      }
      await new Promise((resolve) => setTimeout(resolve, 25));
    } while (Date.now() < deadline);
    throw Error("Timed out: " + expression);
  }
}
function frame(record) {
  const data = Buffer.from(JSON.stringify(record));
  const header =
    data.length < 126
      ? Buffer.from([0x81, data.length])
      : Buffer.from([0x81, 126, data.length >> 8, data.length & 255]);
  return Buffer.concat([header, data]);
}

test(
  "IndexedDB capture, retention settings, streaming imports and bounded display",
  { timeout: 120000 },
  async (t) => {
    const html = await readFile(
      new URL("../assets/log_viewer.html", import.meta.url),
    );
    const server = createServer((request, response) => {
      response.writeHead(200, { "Content-Type": "text/html" });
      response.end(html);
    });
    const connections = new Set();
    server.on("connection", (socket) => {
      connections.add(socket);
      socket.on("close", () => connections.delete(socket));
    });
    server.on("upgrade", (request, socket) => {
      const accept = createHash("sha1")
        .update(
          request.headers["sec-websocket-key"] +
            "258EAFA5-E914-47DA-95CA-C5AB0DC85B11",
        )
        .digest("base64");
      socket.write(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " +
          accept +
          "\r\n\r\n",
      );
      for (let i = 0; i < 40; i++)
        socket.write(
          frame({
            event: "proxy:live",
            fields: { req_id: i, message: "live-" + i },
          }),
        );
      socket.end(Buffer.from([0x88, 0]));
      socket.on("error", () => {});
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const url = "http://127.0.0.1:" + server.address().port;
    const profile = await mkdtemp(join(tmpdir(), "shadowquic-viewer-test-"));
    const chrome = spawn(
      process.env.CHROME_EXECUTABLE_PATH || "google-chrome",
      [
        "--headless=new",
        "--no-sandbox",
        "--disable-gpu",
        "--remote-debugging-port=0",
        "--user-data-dir=" + profile,
        "about:blank",
      ],
      { stdio: ["ignore", "ignore", "pipe"] },
    );
    let browser;
    try {
      const endpoint = await new Promise((resolve, reject) => {
        let stderr = "";
        const timer = setTimeout(
          () => reject(Error("Chrome did not start: " + stderr)),
          15000,
        );
        chrome.on("error", (error) => {
          clearTimeout(timer);
          reject(error);
        });
        chrome.on("exit", (code) => {
          clearTimeout(timer);
          reject(Error("Chrome exited " + code + ": " + stderr));
        });
        chrome.stderr.on("data", (data) => {
          stderr = (stderr + data).slice(-65536);
          const match = stderr.match(/DevTools listening on (ws:\/\/\S+)/);
          if (match) {
            clearTimeout(timer);
            resolve(match[1]);
          }
        });
      });
      const socket = new WebSocket(endpoint);
      await new Promise((resolve, reject) => {
        socket.addEventListener("open", resolve, { once: true });
        socket.addEventListener("error", reject, { once: true });
      });
      browser = new Browser(socket);
      const { targetId } = await browser.call("Target.createTarget", {
        url: "about:blank",
      });
      const { sessionId: session } = await browser.call(
        "Target.attachToTarget",
        { targetId, flatten: true },
      );
      await browser.call("Page.enable", {}, session);
      await browser.call("Page.navigate", { url }, session);
      await browser.wait(
        session,
        'typeof state !== "undefined" && state.ready && Boolean(state.view)',
      );
      const evaluate = (expression) => browser.evaluate(session, expression);
      const reload = async () => {
        const origin = await evaluate("performance.timeOrigin");
        await browser.call("Page.reload", {}, session);
        await browser.wait(
          session,
          `performance.timeOrigin !== ${origin} && typeof state !== "undefined" && state.ready && Boolean(state.view)`,
        );
      };

      await t.test(
        "large captures live in IndexedDB while rows, markers and groups stay bounded",
        async () => {
          const result = await evaluate(`(async () => {
        await state.store.setLimit(state.captureId, 60000); state.maxRecords = 60000;
        for (let batch = 0; batch < 110; batch++) {
          const records = Array.from({length: 500}, (_, i) => normalize({event: 'proxy:test', timestamp: new Date(1700000000000 + batch*500 + i).toISOString(), level: i%2 ? 'INFO' : 'WARN', fields: {req_id: batch*500+i, message: 'event-'+(batch*500+i)}}));
          await state.store.append(state.captureId, records);
        }
        await render();
        const tx = state.store.db.transaction('events'), done = idbDone(tx), count = await idbRequest(tx.objectStore('events').count()); await done;
        return {count, displayed: state.view.capture.count, rows: state.view.rows.length, markers: state.view.markers.length, groups: state.view.groups.size, inMemoryCapture: Object.hasOwn(state, 'records')};
      })()`);
          assert.deepEqual(result, {
            count: 55000,
            displayed: 55000,
            rows: 500,
            markers: 5000,
            groups: 200,
            inMemoryCapture: false,
          });
        },
      );
      await t.test(
        "indexed request queries and combined text filters inspect older retained events",
        async () => {
          const result = await evaluate(`(async () => {
        const filter = {group: 'req:100', category: '', level: '', query: '', requestSearch: '', recordId: null};
        const request = await state.store.query(state.captureId, filter);
        const combined = await state.store.query(state.captureId, {...filter, group: 'all', category: 'proxy', level: 'WARN', query: 'event-54998'});
        return {requestCount: request.count, request: request.rows[0].raw.fields.message, combinedCount: combined.count, combined: combined.rows[0].raw.fields.message};
      })()`);
          assert.deepEqual(result, {
            requestCount: 1,
            request: "event-100",
            combinedCount: 1,
            combined: "event-54998",
          });
        },
      );
      await t.test(
        "lowering the setting prunes oldest records and group metadata immediately",
        async () => {
          await evaluate(
            `(async () => { $('maxRecords').value = '75'; await applyLimit({preventDefault(){}}); })()`,
          );
          await browser.wait(session, "state.view?.capture.count === 75");
          const result = await evaluate(`(async () => {
        const tx = state.store.db.transaction(['events','groups','settings']), done = idbDone(tx);
        const events = await idbRequest(tx.objectStore('events').getAll());
        const groups = await idbRequest(tx.objectStore('groups').getAll());
        const limit = await idbRequest(tx.objectStore('settings').get('maxRecords')); await done;
        return {count: events.length, first: events[0].raw.fields.message, last: events.at(-1).raw.fields.message, groups: groups.length, limit, evicted: state.view.capture.evicted};
      })()`);
          assert.deepEqual(result, {
            count: 75,
            first: "event-54925",
            last: "event-54999",
            groups: 75,
            limit: 75,
            evicted: 54925,
          });
          await reload();
          assert.deepEqual(
            await evaluate(
              `({count: state.view.capture.count, limit: $('maxRecords').value})`,
            ),
            { count: 75, limit: "75" },
          );
        },
      );
      await t.test(
        "failed limit updates roll back the preference and capture together",
        async () => {
          const result = await evaluate(`(async () => {
        const original = state.store.appendInTransaction;
        state.store.appendInTransaction = async () => { throw new DOMException('test quota', 'QuotaExceededError'); };
        try { await state.store.setLimit(state.captureId, 1); } catch {} finally { state.store.appendInTransaction = original; }
        const tx = state.store.db.transaction('settings'), done = idbDone(tx), limit = await idbRequest(tx.objectStore('settings').get('maxRecords')); await done;
        return {limit, count: (await state.store.query(state.captureId, {group:'all', category:'',level:'',query:'',requestSearch:'',recordId:null})).capture.count};
      })()`);
          assert.deepEqual(result, { limit: 75, count: 75 });
        },
      );
      await t.test(
        "streamed files above the former 25 MiB limit import without file.text()",
        async () => {
          const result = await evaluate(`(async () => {
        const raw = JSON.stringify({event:'proxy:large_import', timestamp:'2026-11-07T08:00:00Z', fields:{req_id:7,message:'😀'.repeat(400)}})+'\\n';
        const file = new File([raw.repeat(18000)], 'large.jsonl');
        file.text = () => { throw Error('whole-file read forbidden'); };
        await importFile(file);
        return {size: file.size, count: state.view.capture.count, evicted: state.view.capture.evicted, message: state.view.rows.at(-1).raw.fields.message.length, captures: await idbRequest(state.store.db.transaction('captures').objectStore('captures').count())};
      })()`);
          assert.ok(result.size > 25 * 1024 * 1024);
          assert.deepEqual(
            { ...result, size: 0 },
            { size: 0, count: 75, evicted: 17925, message: 800, captures: 1 },
          );
        },
      );
      await t.test(
        "malformed imports preserve the existing capture and remove staging data",
        async () => {
          const result = await evaluate(`(async () => {
        const before = state.captureId;
        await importFile(new File([(JSON.stringify({event:'proxy:staged'})+'\\n').repeat(1001)+'{broken'], 'broken.jsonl'));
        const tx = state.store.db.transaction(['events','captures']), done = idbDone(tx);
        const count = await idbRequest(tx.objectStore('events').count()), captures = await idbRequest(tx.objectStore('captures').count()); await done;
        return {unchanged: before === state.captureId, count, captures, error: $('notice').textContent.includes('Could not open capture')};
      })()`);
          assert.deepEqual(result, {
            unchanged: true,
            count: 75,
            captures: 1,
            error: true,
          });
        },
      );
      await t.test(
        "JSON arrays and wrappers stream into the same retained capture",
        async () => {
          const result = await evaluate(`(async () => {
        await importFile(new File([JSON.stringify({note:'metadata',records:[{event:'dns:one',fields:{req_id:0}},{event:'api:logs_lagged',dropped:9}],after:true})], 'wrapper.json'));
        return {count:state.view.capture.count,dropped:state.view.capture.dropped,requests:state.view.capture.requestCount};
      })()`);
          assert.deepEqual(result, { count: 2, dropped: 9, requests: 1 });
        },
      );
      await t.test(
        "export reads every stored record independently of filters",
        async () => {
          const result = await evaluate(`(async () => {
        window.exported = [];
        window.showSaveFilePicker = async () => ({createWritable: async () => ({write: async chunk => exported.push(chunk), close: async () => {}, abort: async () => {}})});
        $('search').value='dns:one'; await render();
        await exportCapture();
        return {filtered:state.view.count,events:exported.join('').trim().split('\\n').map(JSON.parse).map(r=>r.event)};
      })()`);
          assert.deepEqual(result, {
            filtered: 1,
            events: ["dns:one", "api:logs_lagged"],
          });
        },
      );
      await t.test(
        "live stream initialization retains messages even when the socket closes immediately",
        async () => {
          await evaluate(
            `(async () => { $('maxRecords').value='20'; await applyLimit({preventDefault(){}}); $('endpoint').value=${JSON.stringify(url.replace("http:", "ws:") + "/api/v1/logs")}; $('secret').value='test-secret-not-persisted'; connect({preventDefault(){}}); })()`,
          );
          await browser.wait(
            session,
            `state.view?.capture.count === 20 && state.view.capture.source.startsWith('Live') && !state.pump`,
          );
          const result = await evaluate(`(async () => {
        const tx=state.store.db.transaction(['events','captures','settings']),done=idbDone(tx);
        const records=await idbRequest(tx.objectStore('events').getAll()),captures=await idbRequest(tx.objectStore('captures').getAll()),settings=await idbRequest(tx.objectStore('settings').getAll()); await done;
        return {count:records.length,first:records[0].raw.fields.message,last:records.at(-1).raw.fields.message,evicted:state.view.capture.evicted,secretStored:JSON.stringify([records,captures,settings]).includes('test-secret-not-persisted')};
      })()`);
          assert.deepEqual(result, {
            count: 20,
            first: "live-20",
            last: "live-39",
            evicted: 20,
            secretStored: false,
          });
        },
      );
      await t.test(
        "pause keeps a bounded display snapshot while retention and capture continue",
        async () => {
          const result = await evaluate(`(async()=>{
        $('pauseButton').click(); const before=state.paused.capture.count;
        addRecords(Array.from({length:30},(_,i)=>normalize({event:'proxy:paused',fields:{req_id:100+i,message:'paused-'+i}})));
        await flushRecords(); await render();
        const tx=state.store.db.transaction('events'),done=idbDone(tx),records=await idbRequest(tx.objectStore('events').getAll()); await done;
        const frozen=$('eventCount').textContent,disabled=$('search').disabled;
        $('pauseButton').click();
        return {before,frozen,disabled,stored:records.length,last:records.at(-1).raw.fields.message};
      })()`);
          assert.deepEqual(result, {
            before: 20,
            frozen: "20",
            disabled: true,
            stored: 20,
            last: "paused-29",
          });
          await browser.wait(
            session,
            '!state.paused && state.view.rows.at(-1)?.raw.event === "proxy:paused"',
          );
        },
      );
      await t.test(
        "clear removes persisted events but preserves the setting",
        async () => {
          await evaluate("clearCapture()");
          await reload();
          assert.deepEqual(
            await evaluate(
              `({count:state.view.capture.count,limit:$('maxRecords').value})`,
            ),
            { count: 0, limit: "20" },
          );
        },
      );
      await t.test(
        "live write buffering stays bounded and reports overflow",
        async () => {
          const result = await evaluate(`(async () => {
          addRecords(Array.from({length: 6100}, (_, i) => normalize({event:'proxy:burst', fields:{req_id:i,message:'burst-'+i}})));
          const pending = state.pendingRecords.length - state.pendingHead;
          await flushRecords(); await render();
          return {pending, count:state.view.capture.count, overflow:state.view.capture.overflow, last:state.view.rows.at(-1).raw.fields.message};
        })()`);
          assert.ok(result.pending <= 5000);
          assert.deepEqual(
            { ...result, pending: 0 },
            { pending: 0, count: 20, overflow: 1100, last: "burst-6099" },
          );
        },
      );
      await t.test(
        "untimed records and out-of-order timestamps preserve retention statistics",
        async () => {
          const result = await evaluate(`(async () => {
          await clearCapture();
          addRecords([
            normalize({event:'dns:old',timestamp:'2026-11-07T08:00:10Z',fields:{req_id:1}}),
            normalize({event:'api:logs_lagged',dropped:7}),
            normalize({event:'security:new',timestamp:'2026-11-07T08:00:00Z',fields:{req_id:2}})
          ]);
          await flushRecords();
          await render();
          $('category').value='dns';
          $('maxRecords').value='2'; await applyLimit({preventDefault(){}});
          return {count:state.view.capture.count,filtered:state.view.count,category:$('category').value,requests:state.view.capture.requestCount,timed:state.view.timedCount,categories:[...state.view.capture.categories.keys()].sort(),dropped:state.view.capture.dropped,events:state.view.rows.map(r=>r.event)};
        })()`);
          assert.deepEqual(result, {
            count: 2,
            filtered: 2,
            category: "",
            requests: 1,
            timed: 1,
            categories: ["api", "security"],
            dropped: 7,
            events: ["api:logs_lagged", "security:new"],
          });
        },
      );
      await t.test(
        "storage failures are visible and preserve previously committed events",
        async () => {
          const result = await evaluate(`(async () => {
          state.store.append = async () => { throw new DOMException('test disk full','QuotaExceededError'); };
          addRecords([normalize({event:'proxy:not_saved'})]); await state.pump;
          return {failed:state.failed,status:$('storageStatus').textContent,disabled:$('connectButton').disabled,notice:$('notice').textContent.includes('test disk full'),count:await idbRequest(state.store.db.transaction('events').objectStore('events').count())};
        })()`);
          assert.deepEqual(result, {
            failed: true,
            status: "Storage unavailable",
            disabled: true,
            notice: true,
            count: 2,
          });
          await reload();
          assert.equal(await evaluate("state.view.capture.count"), 2);
        },
      );
      await t.test(
        "standalone file URLs can persist captures without an HTTP server",
        async () => {
          const { targetId } = await browser.call("Target.createTarget", {
            url: "about:blank",
          });
          const { sessionId: fileSession } = await browser.call(
            "Target.attachToTarget",
            { targetId, flatten: true },
          );
          await browser.call("Page.enable", {}, fileSession);
          await browser.call(
            "Page.navigate",
            { url: new URL("../assets/log_viewer.html", import.meta.url).href },
            fileSession,
          );
          await browser.wait(
            fileSession,
            'typeof state !== "undefined" && state.ready && Boolean(state.view)',
          );
          await browser.evaluate(fileSession, "loadDemo()");
          await browser.wait(fileSession, "state.view?.capture.count === 36");
          const origin = await browser.evaluate(
            fileSession,
            "performance.timeOrigin",
          );
          await browser.call("Page.reload", {}, fileSession);
          await browser.wait(
            fileSession,
            `performance.timeOrigin !== ${origin} && typeof state !== "undefined" && state.ready && state.view?.capture.count === 36`,
          );
          await browser.call("Target.closeTarget", { targetId });
        },
      );
      await t.test(
        "unavailable IndexedDB disables capture with a clear error",
        async () => {
          const { targetId } = await browser.call("Target.createTarget", {
            url: "about:blank",
          });
          const { sessionId: blockedSession } = await browser.call(
            "Target.attachToTarget",
            { targetId, flatten: true },
          );
          await browser.call("Page.enable", {}, blockedSession);
          await browser.call(
            "Page.addScriptToEvaluateOnNewDocument",
            {
              source:
                'Object.defineProperty(window,"indexedDB",{value:undefined});',
            },
            blockedSession,
          );
          await browser.call("Page.navigate", { url }, blockedSession);
          await browser.wait(
            blockedSession,
            'typeof state !== "undefined" && state.failed',
          );
          assert.deepEqual(
            await browser.evaluate(
              blockedSession,
              '({disabled:$("connectButton").disabled,notice:$("notice").textContent.includes("IndexedDB is unavailable")})',
            ),
            { disabled: true, notice: true },
          );
          await browser.call("Target.closeTarget", { targetId });
        },
      );
    } finally {
      if (browser) {
        await browser.call("Browser.close").catch(() => {});
        browser.socket.close();
      }
      chrome.kill();
      for (const socket of connections) socket.destroy();
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
      await rm(profile, {
        recursive: true,
        force: true,
        maxRetries: 10,
        retryDelay: 100,
      });
    }
  },
);
