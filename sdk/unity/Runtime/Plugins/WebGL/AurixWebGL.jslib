// Unity WebGL plugin: the JavaScript half of Aurix.WebGL.NativeWebGLBridge. It loads the standalone
// Web SDK bundle (aurix-web-sdk.js, `window.AurixWebSdk`) and forwards handle-based JSON calls to one
// `AurixWebSdk.AurixBridge` instance. Strings cross the wasm boundary as UTF-8 through Unity's
// UTF8ToString / stringToUTF8 helpers; every returned string is malloc'ed for the C# marshaller,
// which frees it after copying (the Unity convention for `string` returns from `__Internal`).
var AurixWebGLPlugin = {
  $Aurix: {
    bridge: null,
    status: 0, // 0 not loaded, 1 loading, 2 ready, 3 failed (Aurix.WebGL.WebGLSdkStatus)
    error: '',
    script: null,

    ready: function () {
      if (Aurix.bridge) return true;
      var sdk = typeof window !== 'undefined' ? window.AurixWebSdk : undefined;
      if (!sdk || typeof sdk.AurixBridge !== 'function') return false;
      Aurix.bridge = new sdk.AurixBridge();
      Aurix.status = 2;
      Aurix.error = '';
      return true;
    },

    load: function (url) {
      if (Aurix.ready()) return;
      if (Aurix.status === 1) return;
      if (typeof document === 'undefined') {
        Aurix.status = 3;
        Aurix.error = 'no document: the Aurix WebGL bridge needs a browser page';
        return;
      }
      Aurix.status = 1;
      Aurix.error = '';
      var tag = document.createElement('script');
      tag.async = true;
      tag.src = url;
      tag.onload = function () {
        if (!Aurix.ready()) {
          Aurix.status = 3;
          Aurix.error = 'loaded ' + url + ' but window.AurixWebSdk.AurixBridge is missing';
        }
      };
      tag.onerror = function () {
        Aurix.status = 3;
        Aurix.error = 'failed to load ' + url;
        if (tag.parentNode) tag.parentNode.removeChild(tag);
        Aurix.script = null;
      };
      Aurix.script = tag;
      document.head.appendChild(tag);
    },

    out: function (s) {
      var text = typeof s === 'string' ? s : String(s);
      var size = lengthBytesUTF8(text) + 1;
      var ptr = _malloc(size);
      stringToUTF8(text, ptr, size);
      return ptr;
    },

    failure: function (message) {
      return JSON.stringify({ ok: false, error: { message: String(message), name: 'Error' } });
    },
  },

  AurixWebGL_LoadSdk: function (urlPtr) {
    Aurix.load(UTF8ToString(urlPtr));
  },

  AurixWebGL_SdkStatus: function () {
    if (Aurix.status !== 2 && Aurix.ready()) return 2;
    return Aurix.status;
  },

  AurixWebGL_SdkError: function () {
    return Aurix.out(Aurix.error);
  },

  AurixWebGL_Create: function (optionsPtr) {
    if (!Aurix.ready()) return 0;
    try {
      return Aurix.bridge.create(UTF8ToString(optionsPtr));
    } catch (e) {
      Aurix.error = e && e.message ? e.message : String(e);
      return 0;
    }
  },

  AurixWebGL_Invoke: function (handle, methodPtr, argsPtr, rid) {
    if (!Aurix.ready()) return Aurix.out(Aurix.failure('Aurix Web SDK is not loaded'));
    try {
      return Aurix.out(Aurix.bridge.invoke(handle, UTF8ToString(methodPtr), UTF8ToString(argsPtr), rid));
    } catch (e) {
      return Aurix.out(Aurix.failure(e && e.message ? e.message : e));
    }
  },

  AurixWebGL_Drain: function (handle) {
    if (!Aurix.ready()) return Aurix.out('[]');
    try {
      return Aurix.out(Aurix.bridge.drain(handle));
    } catch (e) {
      return Aurix.out('[]');
    }
  },

  AurixWebGL_Destroy: function (handle) {
    if (!Aurix.ready()) return;
    try {
      Aurix.bridge.destroy(handle);
    } catch (e) {
      // already gone
    }
  },
};

autoAddDeps(AurixWebGLPlugin, '$Aurix');
mergeInto(LibraryManager.library, AurixWebGLPlugin);
