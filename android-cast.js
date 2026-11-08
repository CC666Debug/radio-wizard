// Only used inside the RadioWizard Android app. Casting there goes through Android's own
// Google Cast service (like YouTube Music), which doesn't have Chrome's speaker-list bug.
// This file stands in for Google's web cast library with the same names the page already uses,
// passing each call to the app (window.AndroidCast) and turning the app's reports back into events.
// (Based on Four Realms' copy; this one also sends songs (BUFFERED) with a start time, for Bandcamp.)
(function () {
  const bridge = window.AndroidCast;
  let nextId = 1;
  const pending = {};   // command id -> { ok, fail }
  function call(method, args) {
    return new Promise((ok, fail) => {
      const id = nextId++;
      pending[id] = { ok, fail };
      bridge[method](id, JSON.stringify(args || {}));
    });
  }

  const PlayerState = { IDLE: 'IDLE', PLAYING: 'PLAYING', PAUSED: 'PAUSED', BUFFERING: 'BUFFERING' };
  const IdleReason = { CANCELLED: 'CANCELLED', INTERRUPTED: 'INTERRUPTED', FINISHED: 'FINISHED', ERROR: 'ERROR' };
  const CastState = { NO_DEVICES_AVAILABLE: 'NO_DEVICES_AVAILABLE', NOT_CONNECTED: 'NOT_CONNECTED', CONNECTING: 'CONNECTING', CONNECTED: 'CONNECTED' };
  const SessionState = {
    NO_SESSION: 'NO_SESSION', SESSION_STARTING: 'SESSION_STARTING', SESSION_STARTED: 'SESSION_STARTED',
    SESSION_START_FAILED: 'SESSION_START_FAILED', SESSION_ENDING: 'SESSION_ENDING', SESSION_ENDED: 'SESSION_ENDED',
    SESSION_RESUMED: 'SESSION_RESUMED',
  };
  const CastContextEventType = { CAST_STATE_CHANGED: 'caststatechanged', SESSION_STATE_CHANGED: 'sessionstatechanged' };
  const RemotePlayerEventType = { ANY_CHANGE: 'anyChanged' };

  // What the speaker is doing, as last reported by the app.
  const state = { castState: CastState.NO_DEVICES_AVAILABLE, device: null, media: null, volume: null, muted: false };   // volume stays unknown until the speaker reports it

  function listeners() {
    const map = {};
    return {
      add(type, fn) { (map[type] = map[type] || []).push(fn); },
      fire(type, e) { (map[type] || []).forEach(fn => { try { fn(e); } catch (err) { setTimeout(() => { throw err; }); } }); },
    };
  }
  const ctxEvents = listeners();
  const playerEvents = listeners();

  const mediaSession = {
    get idleReason() { return state.media && state.media.idleReason; },
    pause(req, ok, fail) { call('pause').then(() => ok && ok(), e => fail && fail(e)); },
    play(req, ok, fail) { call('play').then(() => ok && ok(), e => fail && fail(e)); },
  };
  const session = {
    getCastDevice: () => ({ friendlyName: state.device || 'speaker' }),
    getMediaSession: () => (state.media ? mediaSession : null),
    getSessionState: () => (state.device ? SessionState.SESSION_STARTED : SessionState.NO_SESSION),
    canQueue: !!bridge.queueAdd,   // app versions that can hand the speaker a queue of songs
    queueAdd: items => call('queueAdd', { items }),
    loadMedia: req => call('load', {
      queue: !!(req.queue && bridge.queueAdd),
      url: req.media.contentId, contentType: req.media.contentType,
      streamType: req.media.streamType || 'LIVE', currentTime: req.currentTime || 0,
      title: req.media.metadata && req.media.metadata.title || '',
      subtitle: req.media.metadata && req.media.metadata.subtitle || '',
      image: req.media.metadata && req.media.metadata.images && req.media.metadata.images[0] && req.media.metadata.images[0].url || '',
    }),
  };

  const context = {
    setOptions() {},
    addEventListener: (type, fn) => ctxEvents.add(type, fn),
    getCastState: () => state.castState,
    getCurrentSession: () => (state.device ? session : null),
    requestSession: () => call('pickSpeaker'),
    endCurrentSession: stopCasting => { call('endSession', { stopCasting: !!stopCasting }); },
  };

  function RemotePlayer() { remotePlayer = this; syncPlayer(); }
  let remotePlayer = null;
  function syncPlayer() {
    if (!remotePlayer) return;
    const m = state.media;
    remotePlayer.isConnected = !!state.device;
    remotePlayer.isMediaLoaded = !!(m && m.playerState && m.playerState !== PlayerState.IDLE) || !!(m && m.contentId);
    remotePlayer.playerState = m ? m.playerState : null;
    remotePlayer.isPaused = !!m && m.playerState === PlayerState.PAUSED;
    remotePlayer.mediaInfo = m && m.contentId ? { contentId: m.contentId } : null;
    remotePlayer.volumeLevel = state.volume;
    remotePlayer.isMuted = state.muted;
  }
  function RemotePlayerController() {}
  RemotePlayerController.prototype.addEventListener = (type, fn) => playerEvents.add(type, fn);
  RemotePlayerController.prototype.stop = () => { call('stop'); };
  RemotePlayerController.prototype.playOrPause = () => { call(state.media && state.media.playerState === PlayerState.PAUSED ? 'play' : 'pause'); };
  RemotePlayerController.prototype.muteOrUnmute = () => { call('setMuted', { muted: !state.muted }); };
  RemotePlayerController.prototype.setVolumeLevel = () => { call('setVolume', { level: remotePlayer ? remotePlayer.volumeLevel : state.volume }); };

  // Reports from the app.
  window.AndroidCastEvent = function (e) {
    if (e.type === 'result') {
      const p = pending[e.id];
      delete pending[e.id];
      if (p) (e.ok ? p.ok() : p.fail(e.error || 'session_error'));
      return;
    }
    if (e.type === 'castState') {
      state.castState = e.castState;
      ctxEvents.fire(CastContextEventType.CAST_STATE_CHANGED, { castState: e.castState });
      return;
    }
    if (e.type === 'session') {
      if (e.device) state.device = e.device;
      if (e.sessionState === SessionState.SESSION_ENDED || e.sessionState === SessionState.SESSION_START_FAILED) {
        state.device = null; state.media = null;
        syncPlayer(); playerEvents.fire(RemotePlayerEventType.ANY_CHANGE, {});
      }
      ctxEvents.fire(CastContextEventType.SESSION_STATE_CHANGED, {
        sessionState: e.sessionState, errorCode: e.errorCode || null, session: state.device ? session : null,
      });
      return;
    }
    if (e.type === 'status') {
      state.media = e.media || null;
      if (e.volume != null) state.volume = e.volume;
      if (e.muted != null) state.muted = e.muted;
      syncPlayer();
      playerEvents.fire(RemotePlayerEventType.ANY_CHANGE, {});
    }
  };

  // The same names Google's web cast library provides.
  window.chrome = window.chrome || {};
  window.chrome.cast = {
    AutoJoinPolicy: { ORIGIN_SCOPED: 'origin_scoped' },
    Image: function (url) { this.url = url; },
    media: {
      DEFAULT_MEDIA_RECEIVER_APP_ID: 'CC1AD845',
      PlayerState, IdleReason,
      StreamType: { LIVE: 'LIVE', BUFFERED: 'BUFFERED' },
      MediaInfo: function (contentId, contentType) { this.contentId = contentId; this.contentType = contentType; },
      GenericMediaMetadata: function () {},
      LoadRequest: function (media) { this.media = media; },
      PauseRequest: function () {},
      PlayRequest: function () {},
    },
  };
  window.cast = {
    framework: {
      CastContext: { getInstance: () => context },
      RemotePlayer, RemotePlayerController,
      CastState, SessionState, CastContextEventType, RemotePlayerEventType,
    },
  };

  window.__onGCastApiAvailable(true);
  bridge.ready();   // the app now sends the current state (and rejoins a speaker that's still playing)
})();
