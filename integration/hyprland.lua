-- 由 integration/desktops.json 生成；只编辑结构定义，不手改产物。
-- 面向当前 Hyprland 0.57 Lua API；原生 IPC 已接入但未经真机验收。不转换旧 hyprlang 配置。手工合并，不覆盖整个配置。
-- 程序须能由 PATH 找到；不会自动写入用户桌面配置。

-- 原生面板是 Overlay 浮动覆盖层，不使用普通窗口的 float/pin 规则。
hl.layer_rule({
    match = { namespace = "^glosslet$" },
    no_anim = true,
    blur = false,
    blur_popups = false,
    dim_around = false,
    above_lock = 0,
})

hl.on("hyprland.start", function()
    hl.exec_cmd("glosslet")
end)

hl.bind("SUPER + SHIFT + G", hl.dsp.exec_cmd("glosslet lookup"), {
    repeating = false,
    description = "Glosslet — 明确查词",
})
