(() => {
  const PORT = __OMNI_BRIDGE_PORT__, TOKEN = '__OMNI_BRIDGE_TOKEN__';
  let ws, backoff = 500;
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
        ws.send(JSON.stringify({ id: cmd.id, status: resp.status, headers, body: await resp.text() }));
      } catch (e) {
        ws.send(JSON.stringify({ id: cmd.id, error: String(e && e.message || e) }));
      }
    };
  };
  connect();
})();
