import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Pango from 'gi://Pango';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS = 'io.github.AnkRoot.Glosslet.Gnome';
const PATH = '/io/github/AnkRoot/Glosslet/Gnome';
const IFACE = 'io.github.AnkRoot.Glosslet.Gnome1';
const LIMIT = 64 * 1024;

export default class GlossletExtension extends Extension {
    enable() {
        if (!Meta.is_wayland_compositor())
            throw new Error('Glosslet Shell 扩展仅用于原生 Wayland；X11 使用独立前端');
        this._epoch = 1;
        this._primary = 1;
        this._generation = 0;
        this._owner = null;
        this._capture = null;
        this._actor = null;
        this._selection = global.display.get_selection();
        this._focusSignal = global.display.connect('notify::focus-window', () => {
            this._epoch++;
            this._cancel();
            this._call('Changed', '(tt)', [this._window(), this._epoch]);
        });
        this._primarySignal = this._selection.connect('owner-changed', (_selection, type) => {
            if (type === Meta.SelectionType.PRIMARY) {
                this._primary++;
                this._cancel();
            }
        });
        this._watch = Gio.bus_watch_name(Gio.BusType.SESSION, BUS, Gio.BusNameWatcherFlags.NONE,
            (connection, _name, owner) => {
                this._unsubscribe();
                this._connection = connection;
                this._owner = owner;
                this._subscription = connection.signal_subscribe(owner, IFACE, null, PATH, null,
                    Gio.DBusSignalFlags.NONE, (_bus, sender, _path, _iface, name, args) => {
                        if (sender !== this._owner)
                            return;
                        try {
                            this._signal(name, args.deep_unpack());
                        } catch (error) {
                            console.error(`Glosslet：${error.message}`);
                            this._hide();
                        }
                    });
                this._call('Attach', '(tt)', [this._window(), this._epoch]);
            }, () => {
                this._unsubscribe();
                this._owner = null;
                this._hide();
            });
    }

    disable() {
        this._call('Detach', '()', []);
        Gio.bus_unwatch_name(this._watch);
        this._unsubscribe();
        global.display.disconnect(this._focusSignal);
        this._selection.disconnect(this._primarySignal);
        this._hide();
        this._owner = null;
        this._connection = null;
    }

    _unsubscribe() {
        if (this._subscription) {
            this._connection.signal_unsubscribe(this._subscription);
            this._subscription = null;
        }
    }

    _call(method, signature, args) {
        if (!this._owner)
            return;
        this._connection.call(this._owner, PATH, IFACE, method, new GLib.Variant(signature, args),
            null, Gio.DBusCallFlags.NONE, 800, null, (connection, result) => {
                try {
                    connection.call_finish(result);
                } catch (error) {
                    console.error(`Glosslet：${method} 未完成：${error.message}`);
                }
            });
    }

    _window() {
        return global.display.focus_window?.get_stable_sequence() ?? 0;
    }

    _origin() {
        const [x, y] = global.get_pointer();
        const monitors = Main.layoutManager.monitors.filter(m =>
            x >= m.x && x < m.x + m.width && y >= m.y && y < m.y + m.height);
        if (monitors.length !== 1)
            throw new Error('鼠标不属于唯一输出，不猜测位置');
        const m = monitors[0];
        return {window: this._window(), epoch: this._epoch, point: [x, y], bounds: [m.x, m.y, m.width, m.height]};
    }

    _cancel() {
        if (!this._capture)
            return;
        const capture = this._capture;
        this._capture = null;
        capture.cancel.cancel();
        if (capture.timer)
            GLib.Source.remove(capture.timer);
    }

    _signal(name, args) {
        if (name === 'CaptureRequested')
            this._read(args[0]);
        else if (name === 'ProbeRequested') {
            this._call('Located', '(s)', [JSON.stringify({ticket: args[0], origin: this._origin()})]);
        } else if (name === 'ViewChanged')
            this._show(JSON.parse(args[0]), args[1], args[2]);
        else if (name === 'Hidden')
            this._hide();
        else if (name === 'CopyRequested') {
            if (!this._frameReady || args[0] !== this._identity) {
                this._call('Copied', '(sb)', [args[0], false]);
                return;
            }
            let success = false;
            try {
                St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, args[1]);
                success = true;
            } finally {
                this._call('Copied', '(sb)', [args[0], success]);
            }
        }
    }

    _read(ticket) {
        this._cancel();
        const origin = this._origin();
        const capture = {ticket, origin, primary: this._primary, owner: this._owner, cancel: new Gio.Cancellable(), timer: 0};
        this._capture = capture;
        const finish = (text, failure) => {
            if (this._capture !== capture || this._owner !== capture.owner)
                return;
            this._capture = null;
            if (capture.timer)
                GLib.Source.remove(capture.timer);
            if (origin.window !== this._window() || origin.epoch !== this._epoch || capture.primary !== this._primary)
                return;
            this._call('Captured', '(s)', [JSON.stringify({ticket, origin, text, failure})]);
        };
        const types = this._selection.get_mimetypes(Meta.SelectionType.PRIMARY);
        if (!types?.length) {
            finish('', null);
            return;
        }
        const mime = ['text/plain;charset=utf-8', 'text/plain', 'UTF8_STRING'].find(type => types.includes(type));
        if (!mime || origin.window === 0) {
            finish(null, 'unavailable');
            return;
        }
        capture.timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 800, () => {
            capture.timer = 0;
            capture.cancel.cancel();
            finish(null, 'timeout');
            return GLib.SOURCE_REMOVE;
        });
        const output = Gio.MemoryOutputStream.new_resizable();
        this._selection.transfer_async(Meta.SelectionType.PRIMARY, mime, LIMIT + 1, output, capture.cancel,
            (selection, result) => {
                try {
                    selection.transfer_finish(result);
                    output.close(null);
                    const bytes = output.steal_as_bytes().get_data();
                    if (bytes.length > LIMIT)
                        finish(null, 'too_large');
                    else {
                        try {
                            finish(new TextDecoder('utf-8', {fatal: true}).decode(bytes), null);
                        } catch (_error) {
                            finish(null, 'invalid_text');
                        }
                    }
                } catch (_error) {
                    finish(null, 'unavailable');
                }
            });
    }

    _hide() {
        this._frameReady = false;
        this._identity = null;
        this._cancel();
        this._generation++;
        if (this._paintSignal) {
            global.stage.disconnect(this._paintSignal);
            this._paintSignal = null;
        }
        if (this._actor) {
            Main.layoutManager.removeChrome(this._actor);
            this._actor.destroy();
            this._actor = null;
        }
    }

    _show(view, x, y) {
        this._hide();
        const generation = this._generation;
        const hostIdentity = {scene: view.scene, display: view.identity};
        const identity = JSON.stringify(hostIdentity);
        this._identity = identity;
        const light = view.theme === 'light';
        const actor = new St.BoxLayout({vertical: true, width: 256, height: 100, reactive: true,
            style: `background-color: ${light ? '#f0f2f5' : '#1f2126'}; color: ${light ? '#1f1f1f' : '#f0f0f0'};`});
        this._actor = actor;
        const action = (kind, index = null) => {
            if (this._frameReady && this._generation === generation && this._actor === actor)
                this._call('Action', '(s)', [JSON.stringify({identity: hostIdentity, kind, index})]);
        };
        const header = new St.BoxLayout({height: 20});
        const button = (label, kind) => {
            const b = new St.Button({label, can_focus: false, style: 'font-size: 11px; padding: 1px 8px; color: #599ebf;'});
            b.connect('clicked', () => action(kind));
            return b;
        };
        header.add_child(button(view.pinned ? '取消固定' : '固定', 'pin'));
        header.add_child(new St.Label({text: 'Glosslet', x_expand: true, style: 'font-size: 11px; padding-top: 1px;'}));
        header.add_child(button(light ? '深色' : '浅色', 'theme'));
        header.add_child(button('×', 'close'));
        actor.add_child(header);
        const scroll = new St.ScrollView({x_expand: true, y_expand: true, overlay_scrollbars: true});
        const body = new St.BoxLayout({vertical: true, x_expand: true, style: 'padding: 4px 8px;'});
        let previous = null;
        let pressed = null;
        scroll.connect('scroll-event', () => {
            previous = null;
            pressed = null;
            return Clutter.EVENT_PROPAGATE;
        });
        view.lines.forEach((line, index) => {
            const label = new St.Label({text: line.text, x_expand: true, reactive: !line.small,
                style: `font-size: ${line.small ? 11 : 14}px; padding-bottom: ${line.small ? 3 : 6}px;`});
            label.clutter_text.set_line_wrap(true);
            label.clutter_text.set_ellipsize(Pango.EllipsizeMode.NONE);
            label.connect('button-press-event', (_label, event) => {
                if (event.get_button() !== 1 || !this._frameReady)
                    return Clutter.EVENT_PROPAGATE;
                const [px, py] = event.get_coords();
                pressed = {index, x: px, y: py, moved: false};
                return Clutter.EVENT_STOP;
            });
            label.connect('motion-event', (_label, event) => {
                const [px, py] = event.get_coords();
                if (pressed && Math.hypot(px - pressed.x, py - pressed.y) > 4) {
                    pressed.moved = true;
                    previous = null;
                }
                return Clutter.EVENT_PROPAGATE;
            });
            label.connect('button-release-event', (_label, event) => {
                if (event.get_button() !== 1)
                    return Clutter.EVENT_PROPAGATE;
                const start = pressed;
                pressed = null;
                const [px, py] = event.get_coords();
                if (!start || start.index !== index || start.moved ||
                    Math.hypot(px - start.x, py - start.y) > 4 || !this._frameReady) {
                    previous = null;
                    return Clutter.EVENT_STOP;
                }
                if (line.copy)
                    action('term', index);
                else if (view.retry)
                    action('retry');
                else {
                    const time = event.get_time();
                    if (previous && time - previous.time <= 500 && Math.hypot(px - previous.x, py - previous.y) <= 4) {
                        action('sentence');
                        previous = null;
                    } else
                        previous = {x: px, y: py, time};
                }
                return Clutter.EVENT_STOP;
            });
            body.add_child(label);
        });
        scroll.add_child(body);
        actor.add_child(scroll);
        actor.set_position(x, y);
        Main.layoutManager.addTopChrome(actor, {trackFullscreen: true});
        this._paintSignal = global.stage.connect('after-paint', () => {
            if (this._actor !== actor || this._generation !== generation || !actor.mapped)
                return;
            global.stage.disconnect(this._paintSignal);
            this._paintSignal = null;
            this._frameReady = true;
            this._call('Frame', '(s)', [identity]);
        });
        actor.queue_redraw();
    }
}
