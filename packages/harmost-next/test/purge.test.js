import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createPurger } from '../src/purge.js';

/** A purger whose requests are captured instead of sent. */
function stub({ status = 200, body = '{"purged":true,"entries":3}' } = {}) {
  const calls = [];
  const purger = createPurger({
    endpoint: 'http://127.0.0.1:9091',
    token: 'test-token-0123456789abcdef',
    fetch: async (url, init) => {
      calls.push({ url: new URL(url), init });
      return new Response(body, { status, headers: { 'content-type': 'application/json' } });
    },
  });
  return { purger, calls };
}

test('the token travels in a header, never in the URL', async () => {
  // A query string is logged by every proxy, load balancer and access log on
  // the path. A purge token in one is a purge token in a log aggregator.
  const { purger, calls } = stub();
  await purger.purgeTags(['product-42']);
  const [call] = calls;
  assert.equal(call.init.headers.authorization, 'Bearer test-token-0123456789abcdef');
  assert.ok(!call.url.search.includes('token'));
  assert.ok(!call.url.href.includes('test-token'));
});

test('redirects are refused rather than followed', async () => {
  // Following one would re-send the Authorization header to whatever host the
  // redirect names.
  const { purger, calls } = stub();
  await purger.purgeTags(['a']);
  assert.equal(calls[0].init.redirect, 'manual');

  const redirecting = stub({ status: 302 });
  await assert.rejects(() => redirecting.purger.purgeTags(['a']), /redirect/);
});

test('it POSTs, because a GET that invalidates a cache gets crawled', async () => {
  const { purger, calls } = stub();
  await purger.purgeTags(['a']);
  assert.equal(calls[0].init.method, 'POST');
  assert.equal(calls[0].url.pathname, '/purge');
});

test('values are sent verbatim, because Harmost matches them verbatim', async () => {
  // Percent-encoding here would be looked up as the encoded form, match
  // nothing, and report success.
  const { purger, calls } = stub();
  await purger.purgePaths(['/products/iphone']);
  assert.equal(calls[0].url.search, '?path=/products/iphone');
});

test('tags and paths travel in one request', async () => {
  const { purger, calls } = stub();
  await purger.purge({ tags: ['sale'], paths: ['/p/1'] });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url.search, '?tag=sale&path=/p/1');
});

test('duplicates collapse', async () => {
  const { purger, calls } = stub();
  await purger.purgeTags(['a', 'a', 'b']);
  assert.equal(calls[0].url.search, '?tag=a&tag=b');
});

test('purging everything is spelled out', async () => {
  const { purger, calls } = stub();
  await purger.purgeAll();
  assert.equal(calls[0].url.search, '?all=1');
});

test('a value that could not have matched is refused, not mangled', async () => {
  const { purger, calls } = stub();
  // A comma can never appear in a stored tag: the tag header is
  // comma-separated, so Harmost split it before indexing.
  await assert.rejects(() => purger.purgeTags(['a,b']), /comma/);
  // These cannot survive a query string unambiguously.
  await assert.rejects(() => purger.purgeTags(['a&b']), /cannot be sent/);
  await assert.rejects(() => purger.purgeTags(['a b']), /cannot be sent/);
  await assert.rejects(() => purger.purgeTags(['a%2Cb']), /cannot be sent/);
  // The stored path always begins with a slash.
  await assert.rejects(() => purger.purgePaths(['products']), /must be absolute/);
  assert.equal(calls.length, 0, 'a refused purge must not reach the network');
});

test('an empty list is a no-op rather than a purge of everything', async () => {
  // The dangerous shape: purgeTags([]) turning into "purge all".
  const { purger, calls } = stub();
  const result = await purger.purgeTags([]);
  assert.equal(result.entries, 0);
  assert.equal(calls.length, 0);
  await assert.rejects(() => purger.purge({}), /at least one/);
});

test('a failed purge throws rather than leaving stale content served quietly', async () => {
  const failing = stub({ status: 401, body: 'unauthorized' });
  await assert.rejects(() => failing.purger.purgeTags(['a']), /401/);
});

test('it refuses to start without an endpoint or a token', () => {
  assert.throws(() => createPurger({ endpoint: '', token: 't' }), /endpoint/);
  assert.throws(() => createPurger({ endpoint: 'http://x', token: '' }), /token/);
});
