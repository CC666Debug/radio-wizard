// RadioWizard helper: an optional free Cloudflare Worker that anyone can put in their own account.
// RW works without it. With its address pasted into RW (Settings → Helper), SoundCloud songs get links that
// never run out: the speaker asks the helper, and the helper fetches a fresh SoundCloud link right then.
// Locked with a key (Cloudflare secret HELPER_KEY) so only your own devices can use it.
//
//   GET  /ping?k=KEY                     → "ok" (RW checks the address with this)
//   GET  /sc/<trackId>?k=KEY              → 302 to a fresh SoundCloud stream (HLS)
//   GET  /files?k=KEY&app=radiowizard&v=N → { v, files: { name: text } } (just { v } when nothing changed since N)
//   POST /file/<name>.md?k=KEY            → saves that backup file (the same Markdown the backup folder gets)
// Backup files live in the Worker's own free D1 database (binding DB), up to 1 MB each.

const SC_API = 'https://api-v2.soundcloud.com';
const DESK = { 'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36' };
let cid = '';   // SoundCloud's public web key, read from its own site (kept while this copy of the helper runs)

async function scKey(fresh) {
  if (cid && !fresh) return cid;
  const page = await (await fetch('https://soundcloud.com/', { headers: DESK })).text();
  const js = (page.match(/https:\/\/a-v2\.sndcdn\.com\/assets\/[^"]+\.js/g) || []).reverse();
  for (const u of js) {
    const m = (await (await fetch(u, { headers: DESK })).text()).match(/client_id[:=]"?([A-Za-z0-9]{32})/);
    if (m) return (cid = m[1]);
  }
  throw new Error('no key');
}
async function scGet(url) {
  for (let k = 0; k < 2; k++) {
    const id = await scKey(k > 0);
    const r = await fetch(url + (url.includes('?') ? '&' : '?') + 'client_id=' + id, { headers: { ...DESK, Accept: 'application/json' } });
    if ((r.status === 401 || r.status === 403) && k === 0) continue;   // the key changed: read the new one
    if (!r.ok) throw new Error('soundcloud ' + r.status);
    return r.json();
  }
}
async function scStream(id) {
  const d = await scGet(SC_API + '/tracks/' + id);
  const tr = (d.media && d.media.transcodings) || [];
  const pick = tr.find(c => c.format.protocol === 'hls' && /mp4a|aac/.test(c.format.mime_type) && /160/.test(c.preset))
    || tr.find(c => c.format.protocol === 'hls' && /mp4a|aac/.test(c.format.mime_type))
    || tr.find(c => c.format.protocol === 'hls');
  if (!pick) throw new Error('no stream');
  const s = await scGet(pick.url + (d.track_authorization ? '?track_authorization=' + encodeURIComponent(d.track_authorization) : ''));
  if (!s || !s.url) throw new Error('no link');
  return s.url;
}

// Backup files: one row per file in the Worker's own free D1 database (binding DB). One save = one row written.
let tableMade = false;
async function dbReady(env) {
  if (tableMade) return;
  await env.DB.prepare('CREATE TABLE IF NOT EXISTS files (app TEXT NOT NULL, name TEXT NOT NULL, text TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (app, name))').run();
  tableMade = true;
}
// Helpers made before the move kept their files in KV storage (binding FILES): copied over once, left in place.
async function fromKv(env, app) {
  let cursor, at = Date.now(), n = 0;
  do {
    const l = await env.FILES.list({ prefix: 'f:' + app + '-', cursor });
    for (const key of l.keys) {
      const text = await env.FILES.get(key.name);
      if (text == null) continue;
      await env.DB.prepare('INSERT OR IGNORE INTO files (app, name, text, at) VALUES (?, ?, ?, ?)').bind(app, key.name.slice(2 + app.length + 1), text, at).run();
      n++;
    }
    cursor = l.list_complete ? null : l.cursor;
  } while (cursor);
  return n ? at : 0;
}

export default {
  async fetch(req, env) {
    const u = new URL(req.url);
    const cors = { 'Access-Control-Allow-Origin': '*', 'Cache-Control': 'no-store' };
    if (!env.HELPER_KEY || u.searchParams.get('k') !== env.HELPER_KEY) return new Response('no', { status: 403, headers: cors });
    try {
      if (u.pathname === '/ping') return new Response('ok', { headers: cors });
      const sc = u.pathname.match(/^\/sc\/(\d+)$/);
      if (sc) return new Response(null, { status: 302, headers: { ...cors, Location: await scStream(sc[1]) } });
      if (u.pathname === '/files' && env.DB) {
        const app = (u.searchParams.get('app') || '').replace(/[^a-z0-9]/g, '');
        if (!app) return new Response('no app', { status: 400, headers: cors });
        await dbReady(env);
        let v = (await env.DB.prepare('SELECT MAX(at) AS v FROM files WHERE app = ?').bind(app).first('v')) || 0;
        if (!v && env.FILES) v = await fromKv(env, app);   // first run after the move from KV storage: copy those files in
        const json = { ...cors, 'Content-Type': 'application/json' };
        if (u.searchParams.get('v') == String(v)) return new Response(JSON.stringify({ v }), { headers: json });
        const { results } = await env.DB.prepare('SELECT name, text FROM files WHERE app = ?').bind(app).all();
        const files = {};
        for (const r of results) files[app + '-' + r.name] = r.text;
        return new Response(JSON.stringify({ v, files }), { headers: json });
      }
      const f = u.pathname.match(/^\/file\/([a-z0-9]+)-([a-z0-9-]+\.md)$/);
      if (f && req.method === 'POST' && env.DB) {
        const text = await req.text();
        if (text.length > 1e6) return new Response('too big', { status: 413, headers: cors });
        await dbReady(env);
        await env.DB.prepare('INSERT INTO files (app, name, text, at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (app, name) DO UPDATE SET text = ?3, at = ?4')
          .bind(f[1], f[2], text, Date.now()).run();
        return new Response('ok', { headers: cors });
      }
      return new Response('not found', { status: 404, headers: cors });
    } catch (e) {
      return new Response(String(e.message || e), { status: 502, headers: cors });
    }
  },
};
