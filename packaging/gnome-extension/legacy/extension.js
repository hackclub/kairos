// GNOME Shell on Wayland gives other programs no way to ask which window has focus.

const {Gio} = imports.gi;

const INTERFACE = `
<node>
  <interface name="dev.obayy.Kairos.Focus1">
    <method name="Get">
      <arg type="u" direction="out" name="pid"/>
      <arg type="s" direction="out" name="wm_class"/>
      <arg type="s" direction="out" name="title"/>
    </method>
    <method name="GetWindow">
      <arg type="u" direction="out" name="pid"/>
      <arg type="s" direction="out" name="wm_class"/>
      <arg type="s" direction="out" name="title"/>
      <arg type="t" direction="out" name="window"/>
    </method>
    <signal name="Changed"/>
  </interface>
</node>`;

class KairosFocus {
    enable() {
        this._window = null;
        this._titleId = 0;
        this._dbus = Gio.DBusExportedObject.wrapJSObject(INTERFACE, this);
        this._dbus.export(Gio.DBus.session, '/dev/obayy/Kairos');
        this._focusId = global.display.connect('notify::focus-window', () => this._focusChanged());
        this._focusChanged();
    }

    disable() {
        global.display.disconnect(this._focusId);
        this._unwatchTitle();
        this._dbus.unexport();
        this._dbus = null;
    }

    Get() {
        const w = global.display.focus_window;
        if (!w)
            return [0, '', ''];
        return [Math.max(w.get_pid(), 0), w.get_wm_class() || '', w.get_title() || ''];
    }

    // One terminal process can draw several windows, so the daemon needs to know when the window changes too.
    GetWindow() {
        const w = global.display.focus_window;
        if (!w)
            return [0, '', '', 0];
        const [pid, wmClass, title] = this.Get();
        return [pid, wmClass, title, w.get_id()];
    }

    _focusChanged() {
        this._unwatchTitle();
        const w = global.display.focus_window;
        if (w) {
            this._window = w;
            this._titleId = w.connect('notify::title', () => this._dbus.emit_signal('Changed', null));
        }
        this._dbus.emit_signal('Changed', null);
    }

    _unwatchTitle() {
        if (this._window && this._titleId)
            this._window.disconnect(this._titleId);
        this._window = null;
        this._titleId = 0;
    }
}

function init() {
    return new KairosFocus();
}
