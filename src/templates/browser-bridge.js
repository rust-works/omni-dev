(() => {
  const PORT = __OMNI_BRIDGE_PORT__, TOKEN = '__OMNI_BRIDGE_TOKEN__';
  let ws, backoff = 500;
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
    ws = new WebSocket('ws://localhost:' + PORT, [TOKEN]);
    ws.onopen = () => { backoff = 500; console.log('✅ omni-dev bridge connected on ' + PORT); };
    ws.onclose = () => { setTimeout(connect, backoff = Math.min(backoff * 2, 10000)); };
    ws.onerror = () => ws.close();
    ws.onmessage = async (event) => {
      const cmd = JSON.parse(event.data);
      try {
        const resp = await fetch(cmd.url, {
          method: cmd.method || 'GET',
          headers: cmd.headers || {},
          body: cmd.body || undefined,
          credentials: 'include',
        });
        const headers = {};
        resp.headers.forEach((v, k) => { headers[k] = v; });
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
