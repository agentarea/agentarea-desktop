// Tiny client for the AgentArea plugin bridge: aa(method, params) -> Promise.
(function () {
  let seq = 0;
  const pending = new Map();
  window.addEventListener('message', (e) => {
    const m = e.data;
    if (!m || m.type !== 'aa:response' || !pending.has(m.id)) return;
    const p = pending.get(m.id);
    pending.delete(m.id);
    m.error ? p.reject(new Error(m.error)) : p.resolve(m.result);
  });
  window.aa = (method, params) =>
    new Promise((resolve, reject) => {
      const id = ++seq;
      pending.set(id, { resolve, reject });
      parent.postMessage({ type: 'aa:request', id, method, params }, '*');
    });
})();
