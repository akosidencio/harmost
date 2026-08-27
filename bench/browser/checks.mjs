// Browser-driven checks against the Next.js fixture behind Harmost.
//
// Everything else in bench/ speaks to the proxy with curl. That is enough for
// the claims that are about HTTP, but two of Next.js's behaviours are not
// really about HTTP at all — they are about what the framework's own client
// decides to send:
//
//   * A prefetch is issued by the router, with a `Next-Router-State-Tree` that
//     encodes the entire client route state. Hand-writing one proves that a
//     header Harmost invented is handled; it does not prove the real thing is.
//   * A Server Action submission is a POST the React runtime builds, with an
//     action id the build assigns. It cannot be written down in advance,
//     because it changes every build.
//
// So these two are driven by a real browser, and the requests asserted on are
// the ones Chromium actually sent.

import { chromium } from "playwright";

const PROXY = process.env.PROXY_URL || "http://127.0.0.1:18080";
const METRICS = process.env.METRICS_URL || "http://127.0.0.1:19090";

let failures = 0;

function pass(message) {
  console.log(`PASS: ${message}`);
}

function fail(message) {
  console.error(`FAIL: ${message}`);
  failures += 1;
}

function assert(condition, message) {
  if (condition) return true;
  fail(message);
  return false;
}

/// Origin requests for one route, read from Harmost's own counter. The Next
/// fixture reports render ids in the body too, but a prefetch payload has no
/// body a test can read, so the counter is the observable that covers both.
async function originRequests(route) {
  const text = await (await fetch(`${METRICS}/metrics`)).text();
  let sum = 0;
  for (const line of text.split("\n")) {
    if (!line.startsWith("harmost_origin_requests_total{")) continue;
    if (!line.includes(`route="${route}"`)) continue;
    sum += Number(line.trim().split(/\s+/).pop());
  }
  return sum;
}

async function checkPrefetch(browser) {
  const context = await browser.newContext();
  const page = await context.newPage();

  // Record every request the router issues, not just the document.
  const requests = [];
  page.on("request", (request) => {
    requests.push({
      url: request.url(),
      method: request.method(),
      headers: request.headers(),
    });
  });

  await page.goto(`${PROXY}/`, { waitUntil: "networkidle" });
  // The homepage's product link is in the viewport, so the App Router
  // prefetches it. Hovering is belt and braces for a headless viewport.
  await page.hover('a[href="/products/atlas-runner"]').catch(() => {});
  await page.waitForTimeout(1500);

  const prefetches = requests.filter(
    (request) =>
      request.headers["next-router-prefetch"] !== undefined ||
      request.headers["next-router-segment-prefetch"] !== undefined,
  );

  if (
    !assert(
      prefetches.length > 0,
      `the router issued no prefetch request; observed ${requests.length} requests: ` +
        requests.map((r) => r.url).join(", "),
    )
  ) {
    await context.close();
    return;
  }

  const prefetch = prefetches[0];
  assert(
    prefetch.headers.rsc !== undefined,
    "a prefetch arrived without the RSC header, so it would not be keyed as a flight payload",
  );

  // Replay the browser's own prefetch twice. A prefetch payload is keyed on
  // the router state tree — near-unbounded cardinality — so Harmost collapses
  // concurrent duplicates but must never store one. Two sequential replays
  // must therefore cost two renders, not one.
  const replayHeaders = { ...prefetch.headers };
  delete replayHeaders["content-length"];

  const before = await originRequests("products");
  await fetch(prefetch.url, { headers: replayHeaders });
  await fetch(prefetch.url, { headers: replayHeaders });
  const after = await originRequests("products");

  assert(
    after - before === 2,
    `two sequential replays of the browser's own prefetch cost ${after - before} origin renders; ` +
      "a prefetch payload must be coalesced but never stored",
  );

  pass(
    `the App Router prefetched ${new URL(prefetch.url).pathname}; replaying it twice cost two renders, so nothing was stored`,
  );
  await context.close();
}

async function checkServerAction(browser) {
  const context = await browser.newContext();
  const page = await context.newPage();

  const posts = [];
  page.on("request", (request) => {
    if (request.method() === "POST") {
      posts.push({ url: request.url(), headers: request.headers() });
    }
  });

  const readCount = async () => {
    const text = await page.textContent("main");
    const match = /Items:\s*(\d+)/.exec(text || "");
    return match ? Number(match[1]) : null;
  };

  await page.goto(`${PROXY}/cart`, { waitUntil: "networkidle" });
  const start = await readCount();
  if (!assert(start !== null, "could not read the cart count from /cart")) {
    await context.close();
    return;
  }

  const before = await originRequests("private-routes");

  // Two real submissions of a real Server Action form. If either the response
  // or the mutation itself were reused, the count would stop advancing — which
  // is the failure this exists to catch, and the reason a mutation may never
  // be cached or coalesced.
  for (let index = 1; index <= 2; index += 1) {
    await page.click('button[type="submit"]');
    await page.waitForFunction(
      (expected) => {
        const text = document.querySelector("main")?.textContent || "";
        const match = /Items:\s*(\d+)/.exec(text);
        return match ? Number(match[1]) === expected : false;
      },
      start + index,
      { timeout: 10_000 },
    ).catch(() => {});
    const observed = await readCount();
    assert(
      observed === start + index,
      `after submission ${index} the cart read ${observed}, expected ${start + index}; ` +
        "a Server Action response was reused",
    );
  }

  const after = await originRequests("private-routes");
  assert(
    posts.length >= 2,
    `the browser sent ${posts.length} POST requests, expected at least two form submissions`,
  );
  assert(
    posts.every((post) => post.headers["next-action"] !== undefined),
    "a form submission arrived without a Next-Action header, so it would not classify as a mutation",
  );
  assert(
    after - before >= 2,
    `two Server Action submissions reached the origin ${after - before} times; each must render`,
  );

  pass(
    `two real Server Action submissions advanced the cart to ${start + 2}, each reaching the origin`,
  );
  await context.close();
}

const browser = await chromium.launch();
try {
  await checkPrefetch(browser);
  await checkServerAction(browser);
} finally {
  await browser.close();
}

if (failures > 0) {
  console.error(`\n${failures} browser check(s) failed`);
  process.exit(1);
}
console.log("\nPASS: browser-driven Next.js checks completed");
