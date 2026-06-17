(() => {
  const PORT = __OMNI_BRIDGE_PORT__, TOKEN = '__OMNI_BRIDGE_TOKEN__';
  // Use the `localhost` hostname, not a loopback IP: page CSPs commonly allow
  // `ws://localhost:*` in connect-src but not `ws://127.0.0.1`. The bridge binds
  // both 127.0.0.1 and ::1, so this connects whichever way localhost resolves.
  const ENDPOINT = 'ws://localhost:' + PORT;
  const log = (...a) => console.log('%c[omni-dev]%c', 'color:#3ddc84;font-weight:bold', '', ...a);
  const warn = (...a) => console.warn('[omni-dev]', ...a);
  let ws, backoff = 500;
  // Readers of in-flight streamed responses, keyed by command id, so a `cancel`
  // frame from the server can stop one mid-flight.
  const activeStreams = new Map();
  // Only bodies whose Content-Type positively reads as text go back as text;
  // everything else (and a missing type) is base64-encoded so binary bodies
  // (images, gzip blobs, downloads) round-trip intact rather than corrupting.
  const isTextual = (ct) => {
    if (!ct) return false;
    const t = ct.toLowerCase();
    return t.startsWith('text/') || /(json|xml|javascript|ecmascript|x-www-form-urlencoded)/.test(t);
  };
  const toBase64 = (buf) => {
    const bytes = new Uint8Array(buf);
    let bin = '';
    // Chunk to stay clear of the argument-count limit on String.fromCharCode.
    for (let i = 0; i < bytes.length; i += 0x8000) {
      bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    }
    return btoa(bin);
  };
  const connect = () => {
    log('connecting to ' + ENDPOINT + ' …');
    ws = new WebSocket(ENDPOINT, [TOKEN]);
    ws.onopen = () => { backoff = 500; log('connected ✅ — ready for requests'); };
    ws.onerror = () => {
      warn('WebSocket error connecting to ' + ENDPOINT + '. Check: the daemon is running'
        + ' (`omni-dev daemon status`); this page’s Content-Security-Policy `connect-src`'
        + ' allows it; and (on an https page) mixed-content is not blocking ws://.');
      ws.close();
    };
    ws.onclose = (e) => {
      warn('disconnected (code ' + e.code + '); retrying in ' + backoff + 'ms');
      setTimeout(connect, backoff = Math.min(backoff * 2, 10000));
    };
    ws.onmessage = async (event) => {
      const cmd = JSON.parse(event.data);
      // A cancel frame stops an in-flight streamed response by cancelling its reader.
      if (cmd.cancel) {
        const reader = activeStreams.get(cmd.id);
        if (reader) { try { reader.cancel(); } catch (e) {} }
        return;
      }
      try {
        const resp = await fetch(cmd.url, {
          method: cmd.method || 'GET',
          headers: cmd.headers || {},
          body: cmd.body || undefined,
          credentials: cmd.credentials || 'include',
        });
        const headers = {};
        resp.headers.forEach((v, k) => { headers[k] = v; });
        if (cmd.stream) {
          // Streamed response: send a head frame, then base64 chunk frames as
          // the body arrives, then a terminating `done` frame.
          ws.send(JSON.stringify({ id: cmd.id, status: resp.status, headers, stream: true }));
          if (!resp.body) { ws.send(JSON.stringify({ id: cmd.id, done: true })); return; }
          const reader = resp.body.getReader();
          activeStreams.set(cmd.id, reader);
          try {
            for (let seq = 0; ; seq++) {
              const { done, value } = await reader.read();
              if (done) break;
              ws.send(JSON.stringify({ id: cmd.id, seq, chunk: toBase64(value) }));
            }
            ws.send(JSON.stringify({ id: cmd.id, done: true }));
          } finally {
            activeStreams.delete(cmd.id);
          }
          return;
        }
        const reply = { id: cmd.id, status: resp.status, headers };
        if (isTextual(headers['content-type'])) {
          reply.body = await resp.text();
        } else {
          reply.body = toBase64(await resp.arrayBuffer());
          reply.encoding = 'base64';
        }
        ws.send(JSON.stringify(reply));
      } catch (e) {
        ws.send(JSON.stringify({ id: cmd.id, error: String(e && e.message || e) }));
      }
    };
  };
  connect();
})();
