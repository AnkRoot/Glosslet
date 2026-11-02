// lookupShortcut 由 integration/desktops.json 的统一快捷键定义派生。
const bus = 'io.github.AnkRoot.Glosslet.KWin';
const path = '/io/github/AnkRoot/Glosslet/KWin';
const iface = 'io.github.AnkRoot.Glosslet.KWin1';
const generation = String(Date.now()) + '-' + String(Math.random());
let epoch = 1;
let lease = 0;

function windowId() {
    const window = workspace.activeWindow;
    return window ? window.internalId.toString() : '';
}
function attached(callback) {
    callDBus(bus, path, iface, 'Attach', generation, function (value) {
        if (typeof value !== 'string' || !/^[1-9][0-9]*$/.test(value))
            return;
        lease = Number(value);
        callback(lease);
    });
}
function focused() {
    if (lease > 0)
        callDBus(bus, path, iface, 'Focused', JSON.stringify({lease, window: windowId(), epoch, point: null}));
}
workspace.windowActivated.connect(function () {
    epoch++;
    focused();
});
workspace.windowRemoved.connect(function () {
    epoch++;
    focused();
});
attached(focused);

const actions = [
    {id: 'Glosslet-Lookup', label: 'Glosslet — 明确查词', shortcut: lookupShortcut, method: 'Lookup'},
    {id: 'Glosslet-Probe', label: 'Glosslet — 公开面板诊断', shortcut: '', method: 'Probed'},
];
for (const action of actions) {
    registerShortcut(action.id, action.label, action.shortcut, function () {
        // 只在这次明确动作中查询原生鼠标坐标；不订阅 cursorPosChanged。
        attached(function (currentLease) {
            const point = workspace.cursorPos;
            callDBus(bus, path, iface, action.method, JSON.stringify({
                lease: currentLease, window: windowId(), epoch, point: [point.x, point.y],
            }));
        });
    });
}
