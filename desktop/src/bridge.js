// RadioWizard for Windows: gives the page the same window.AndroidCast bridge as the Android app,
// backed by the app's own Rust code (web requests without browser limits, Bandcamp sign-in, Google Cast).
// The page then loads android-cast.js, which turns this bridge into the cast library names it already uses.
(function () {
  if (window.AndroidCast) return;
  const inv = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args || {});
  const fire = e => { if (window.AndroidCastEvent) window.AndroidCastEvent(e); };
  const http = (id, r) => { if (window.AndroidHttp) window.AndroidHttp(id, r); };
  // Links that open a new window (YouTube, Spotify, Bandcamp… in the song lists) go to the PC's browser.
  document.addEventListener('click', e => {
    const a = e.target.closest && e.target.closest('a[href]');
    if (!a || !/^https?:/i.test(a.href) || (a.target !== '_blank' && a.origin === location.origin)) return;
    e.preventDefault();
    inv('open_url', { url: a.href }).catch(() => {});
  }, true);
  const origOpen = window.open;
  window.open = (u, t, f) => { if (u && /^https?:/i.test(String(u))) { inv('open_url', { url: String(u) }).catch(() => {}); return null; } return origOpen ? origOpen.call(window, u, t, f) : null; };
  const result = (id, p) => p.then(() => fire({ type: 'result', id, ok: true }), e => fire({ type: 'result', id, ok: false, error: String(e) }));

  // ── Following the speaker: the app is asked every second, and changes become the usual events ──
  let last = { castState: null, device: null, status: '' }, polling = false, ending = false;
  async function poll() {
    let s;
    try { s = await inv('cast_state'); } catch (e) { return; }
    const castState = s.connected ? 'CONNECTED' : s.connecting ? 'CONNECTING' : s.devices ? 'NOT_CONNECTED' : 'NO_DEVICES_AVAILABLE';
    if (castState !== last.castState) { last.castState = castState; fire({ type: 'castState', castState }); }
    const device = s.connected ? s.device : null;
    if (device && device !== last.device) {
      fire({ type: 'session', sessionState: last.fresh ? 'SESSION_STARTED' : 'SESSION_RESUMED', device });
      last.fresh = false;
    }
    if (!device && last.device) {
      fire({ type: 'session', sessionState: 'SESSION_ENDING', device: last.device });
      fire({ type: 'session', sessionState: 'SESSION_ENDED', errorCode: s.error || (ending ? 'stopped' : 'speaker ended') });
      ending = false;
    }
    last.device = device;
    if (device) {
      const st = { type: 'status', media: s.playerState ? { playerState: s.playerState, idleReason: s.idleReason || null, contentId: s.contentId || null } : null, volume: s.volume, muted: !!s.muted };
      const key = JSON.stringify(st);
      if (key !== last.status) { last.status = key; fire(st); }
    } else last.status = '';
    if (pickerOpen) drawPicker(s);
  }
  function startPolling() { if (polling) return; polling = true; poll(); setInterval(poll, 1000); }

  // ── The speaker list ──
  let pickerOpen = false, pickId = 0, box = null;
  function closePicker(ok, why) {
    pickerOpen = false;
    if (box) { box.remove(); box = null; }
    if (pickId) { const id = pickId; pickId = 0; fire({ type: 'result', id, ok, error: ok ? null : why || 'cancel' }); }
  }
  async function drawPicker(s) {
    if (!box) return;
    const list = await inv('cast_devices').catch(() => []);
    const body = box.querySelector('.rwp-body');
    if (s && s.connected) {
      body.innerHTML = '<p>Casting to <b></b></p><button class="rwp-btn" data-stop>Stop casting</button>';
      body.querySelector('b').textContent = s.device;
      return;
    }
    if (s && s.connecting) { body.innerHTML = '<p>Connecting…</p>'; return; }
    body.innerHTML = list.length ? '' : '<p>Looking for speakers on your network…</p>';
    list.forEach(d => {
      const b = document.createElement('button');
      b.className = 'rwp-dev';
      b.dataset.id = d.id;
      b.innerHTML = '<span></span><small></small>';
      b.firstChild.textContent = d.name;
      b.lastChild.textContent = d.model || '';
      body.appendChild(b);
    });
  }
  function openPicker(id) {
    if (box) closePicker(false);
    pickId = id; pickerOpen = true;
    box = document.createElement('div');
    box.innerHTML = '<style>.rwp-back{position:fixed;inset:0;background:rgba(0,0,0,.6);z-index:99;display:grid;place-items:center}'
      + '.rwp{background:#181329;color:#efeaff;border:1px solid #352b57;border-radius:18px;width:min(420px,calc(100vw - 32px));max-height:80vh;overflow:auto;padding:16px;font:16px system-ui,sans-serif}'
      + '.rwp h3{margin:0 0 10px}.rwp-dev{display:flex;flex-direction:column;align-items:flex-start;width:100%;text-align:left;background:#221b3a;color:inherit;border:1px solid #352b57;border-radius:12px;padding:10px 12px;margin:6px 0;cursor:pointer;font:inherit}'
      + '.rwp-dev:hover{border-color:#9b6bff}.rwp-dev small{color:#a59bc8}.rwp-btn{background:#221b3a;color:inherit;border:1px solid #352b57;border-radius:10px;padding:8px 14px;cursor:pointer;font:inherit}'
      + '.rwp-foot{display:flex;justify-content:flex-end;margin-top:10px}</style>'
      + '<div class="rwp-back"><div class="rwp"><h3>Cast to</h3><div class="rwp-body"></div><div class="rwp-foot"><button class="rwp-btn" data-close>Close</button></div></div></div>';
    document.body.appendChild(box);
    box.addEventListener('click', e => {
      if (e.target.classList.contains('rwp-back') || e.target.closest('[data-close]')) { closePicker(false); return; }
      if (e.target.closest('[data-stop]')) { ending = true; inv('cast_end', { stop: true }); closePicker(false); return; }
      const dev = e.target.closest('.rwp-dev');
      if (dev) {
        last.fresh = true;
        drawPicker({ connecting: true });
        inv('cast_connect', { id: dev.dataset.id }).then(() => closePicker(true), err => { last.fresh = false; closePicker(false, 'session_error (' + err + ')'); });
      }
    });
    drawPicker(null);
  }

  window.AndroidCast = {
    desktop: true,
    // backup folder (a real Windows dialog): the page uses these instead of the WebView's own folder picker
    folderPick: () => inv('backup_pick'),
    folderGet: () => inv('backup_folder'),
    folderRead: name => inv('backup_read', { name }),
    folderWrite: (name, text) => inv('backup_write', { name, text }),
    folderForget: () => inv('backup_forget'),
    httpGet(id, args) { inv('http_get', { args }).then(r => http(id, r), e => http(id, { error: String(e) })); },
    icy(id, args) { inv('icy', { args }).then(r => http(id, r), e => http(id, { ok: false, error: String(e) })); },
    probe(id, args) { inv('probe', { args }).then(r => http(id, r), e => http(id, { ok: false, error: String(e) })); },
    bandcampLogin() { inv('bandcamp_login'); },
    bandcampLogout() { inv('bandcamp_logout'); },
    soundcloudLogin() { inv('soundcloud_login'); },
    soundcloudLogout() { inv('soundcloud_logout'); },
    ready() { startPolling(); },
    pickSpeaker(id) { openPicker(id); },
    load(id, args) { result(id, inv('cast_load', { args })); },
    queueAdd(id, args) { result(id, inv('cast_queue_add', { args })); },
    pause(id) { result(id, inv('cast_media', { what: 'PAUSE' })); },
    play(id) { result(id, inv('cast_media', { what: 'PLAY' })); },
    stop(id) { result(id, inv('cast_media', { what: 'STOP' })); },
    setVolume(id, args) { result(id, inv('cast_volume', { level: JSON.parse(args).level, muted: null })); },
    setMuted(id, args) { result(id, inv('cast_volume', { level: null, muted: !!JSON.parse(args).muted })); },
    endSession(id, args) { ending = true; result(id, inv('cast_end', { stop: JSON.parse(args).stopCasting !== false })); },
    shareText(title, text) { navigator.clipboard && navigator.clipboard.writeText(text); },
  };
})();
