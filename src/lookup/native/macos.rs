//! macOS 仅读取 AXSelectedText；权限不足明确失败，不模拟复制取词。

use super::{Event, Origin};
use crate::core::{Failure, MAX_SELECTION_BYTES, Ticket};
use accessibility_sys::*;
use anyhow::{Result, bail};
use core_foundation::{
    base::{CFType, TCFType},
    runloop::{CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRemoveSource, kCFRunLoopDefaultMode},
    string::CFString,
};
use core_graphics::{
    event::CGEvent,
    event_source::{CGEventSource, CGEventSourceStateID},
};
use objc2::rc::Retained;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeString, NSWorkspace,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{NSObjectProtocol, NSString};
use std::ffi::c_void;
use tokio::runtime::Handle;
use winit::{event_loop::EventLoopProxy, window::Window};

struct Element(CFType);
impl Element {
    fn application(pid: i32) -> std::result::Result<Self, Failure> {
        // SAFETY: Create 返回拥有的 AX 对象，由 CFType 在离开作用域时释放。
        unsafe {
            let pointer = AXUIElementCreateApplication(pid);
            if pointer.is_null() {
                return Err(Failure::CaptureUnavailable);
            }
            let value = Self(CFType::wrap_under_create_rule(pointer.cast()));
            if AXUIElementSetMessagingTimeout(pointer, 0.15) != 0 {
                return Err(Failure::CaptureUnavailable);
            }
            Ok(value)
        }
    }
    fn pointer(&self) -> AXUIElementRef {
        self.0.as_CFTypeRef().cast_mut().cast()
    }
    fn attribute(&self, name: &str) -> std::result::Result<CFType, Failure> {
        let name = CFString::new(name);
        let mut pointer = std::ptr::null();
        // SAFETY: 接收指针有效；只在成功且非空时采用 Create 规则，调用有 AX messaging timeout。
        unsafe {
            if AXUIElementCopyAttributeValue(
                self.pointer(),
                name.as_concrete_TypeRef(),
                &mut pointer,
            ) != 0
                || pointer.is_null()
            {
                return Err(Failure::CaptureUnavailable);
            }
            Ok(CFType::wrap_under_create_rule(pointer))
        }
    }
    fn element(&self, name: &str) -> std::result::Result<Self, Failure> {
        let value = self.attribute(name)?;
        // SAFETY: 仅查询拥有的 CF 对象类型，验证后才能作为 AX 对象使用。
        if value.type_of() != unsafe { AXUIElementGetTypeID() } {
            return Err(Failure::CaptureUnavailable);
        }
        // SAFETY: 类型已验证，返回的 AX 元素也设置有限 IPC 等待。
        if unsafe { AXUIElementSetMessagingTimeout(value.as_CFTypeRef().cast_mut().cast(), 0.15) }
            != 0
        {
            return Err(Failure::CaptureUnavailable);
        }
        Ok(Self(value))
    }
}

struct Observer(CFType);
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: observer 持有 run loop source；先注销再释放对象。
        unsafe {
            CFRunLoopRemoveSource(
                CFRunLoopGetMain(),
                AXObserverGetRunLoopSource(self.0.as_CFTypeRef().cast_mut().cast()),
                kCFRunLoopDefaultMode,
            );
        }
    }
}

pub(super) struct Device {
    proxy: Box<EventLoopProxy<Event>>,
    notification: Retained<objc2::runtime::ProtocolObject<dyn NSObjectProtocol>>,
    observer: Option<Observer>,
}

fn frontmost() -> u64 {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map_or(0, |app| app.processIdentifier() as u64)
}

impl Device {
    pub const NAME: &'static str = "macos";
    pub fn new(proxy: EventLoopProxy<Event>, _: &Handle) -> Result<Self> {
        let events = proxy.clone();
        let block = block2::RcBlock::new(
            move |_: std::ptr::NonNull<objc2_foundation::NSNotification>| {
                let _ = events.send_event(Event::Focus(frontmost()));
            },
        );
        // SAFETY: NSWorkspace 通知在主线程队列执行；block 的捕获由系统保存，Drop 注销 observer。
        let notification = unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .addObserverForName_object_queue_usingBlock(
                    Some(NSWorkspaceDidActivateApplicationNotification),
                    None,
                    None,
                    &block,
                )
        };
        Ok(Self {
            proxy: Box::new(proxy),
            notification,
            observer: None,
        })
    }
    pub fn check() -> Result<()> {
        // SAFETY: 只检查已授予的权限，不弹授权提示，不读取 AX 文本。
        if !unsafe { AXIsProcessTrusted() } {
            bail!("请在系统设置 → 隐私与安全性 → 辅助功能中允许 Glosslet")
        }
        println!("桌面：macOS；辅助功能访问已允许；未读取选区、未注册快捷键、未映射窗口。");
        Ok(())
    }
    pub fn origin(&self) -> Result<Origin> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| anyhow::anyhow!("无法查询原生鼠标位置"))?;
        let event = CGEvent::new(source).map_err(|_| anyhow::anyhow!("无法查询原生鼠标位置"))?;
        let point = event.location();
        Ok(Origin {
            source: frontmost(),
            point: (point.x, point.y),
            logical: true,
        })
    }
    pub fn capture(&mut self, ticket: Ticket) -> Result<()> {
        let origin = self.origin()?;
        let text = self.text(origin.source);
        let _ = self.proxy.send_event(Event::Captured {
            ticket,
            origin,
            text,
        });
        Ok(())
    }
    fn text(&mut self, pid: u64) -> std::result::Result<String, Failure> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
        let check = || {
            if std::time::Instant::now() < deadline {
                Ok(())
            } else {
                Err(Failure::CaptureTimeout)
            }
        };
        // SAFETY: 仅查询已有权限；没有隐式请求 Screen Recording 或 Input Monitoring。
        if !unsafe { AXIsProcessTrusted() } {
            return Err(Failure::PermissionRequired);
        }
        if pid == 0 || pid == std::process::id() as u64 {
            return Err(Failure::CaptureUnavailable);
        }
        let app = Element::application(pid as i32)?;
        let focused = app.element("AXFocusedUIElement")?;
        let window = app.element("AXFocusedWindow")?;
        check()?;
        self.observe(pid as i32, &app, &window)?;
        check()?;
        let text = focused
            .attribute("AXSelectedText")?
            .downcast::<CFString>()
            .ok_or(Failure::InvalidText)?;
        if text.char_len() as usize > MAX_SELECTION_BYTES {
            return Err(Failure::SelectionTooLarge);
        }
        let text = text.to_string();
        if text.len() > MAX_SELECTION_BYTES {
            return Err(Failure::SelectionTooLarge);
        }
        check()?;
        if frontmost() != pid
            || app.element("AXFocusedUIElement")?.0 != focused.0
            || app.element("AXFocusedWindow")?.0 != window.0
        {
            return Err(Failure::CaptureUnavailable);
        }
        check()?;
        Ok(text)
    }
    fn observe(
        &mut self,
        pid: i32,
        app: &Element,
        window: &Element,
    ) -> std::result::Result<(), Failure> {
        self.observer = None;
        let mut pointer = std::ptr::null_mut();
        // SAFETY: observer 与 Box 内代理由此对象独占。Box 地址稳定，Drop 先注销 source，后销毁代理。
        unsafe {
            if AXObserverCreate(pid, invalidated, &mut pointer) != 0 || pointer.is_null() {
                return Err(Failure::CaptureUnavailable);
            }
            let observer = Observer(CFType::wrap_under_create_rule(pointer.cast()));
            for (element, name) in [
                (app, "AXFocusedWindowChanged"),
                (window, "AXUIElementDestroyed"),
            ] {
                let name = CFString::new(name);
                if AXObserverAddNotification(
                    pointer,
                    element.pointer(),
                    name.as_concrete_TypeRef(),
                    (&mut *self.proxy as *mut EventLoopProxy<Event>).cast(),
                ) != 0
                {
                    return Err(Failure::CaptureUnavailable);
                }
            }
            CFRunLoopAddSource(
                CFRunLoopGetMain(),
                AXObserverGetRunLoopSource(pointer),
                kCFRunLoopDefaultMode,
            );
            self.observer = Some(observer);
        }
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.observer = None;
    }
    pub fn owns(&self, source: u64, _: &Window) -> bool {
        source == std::process::id() as u64
    }
    pub fn show(&self, window: &Window) -> Result<()> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
            bail!("不是 AppKit 窗口")
        };
        // SAFETY: NSView 仍由 Winit window 持有，调用仅在 AppKit 主线程执行；orderFront 不激活应用。
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<objc2_app_kit::NSView>() };
        view.window()
            .ok_or_else(|| anyhow::anyhow!("AppKit 窗口不可用"))?
            .orderFront(None);
        Ok(())
    }
    pub fn hide(&self, window: &Window) {
        window.set_visible(false);
    }
    pub fn focus(&self, window: &Window) -> Result<()> {
        window.focus_window();
        Ok(())
    }
    pub fn copy(&mut self, text: String, _: &Window) -> Result<()> {
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        // SAFETY: 只写入用户明确复制的译文，不读取或恢复原剪贴板。
        if !pasteboard.setString_forType(&NSString::from_str(&text), unsafe {
            NSPasteboardTypeString
        }) {
            bail!("无法写入 macOS 剪贴板");
        }
        Ok(())
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        self.observer = None;
        // SAFETY: notification 仍被此对象持有；先从中心注销再释放。
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .removeObserver(AsRef::<objc2::runtime::AnyObject>::as_ref(
                    &*self.notification,
                ));
        }
    }
}
unsafe extern "C" fn invalidated(
    _: AXObserverRef,
    _: AXUIElementRef,
    _: core_foundation::string::CFStringRef,
    data: *mut c_void,
) {
    // SAFETY: observer 的 userdata 指向 Device 拥有的稳定 Box，注销后不再调用。
    let proxy = unsafe { &*data.cast::<EventLoopProxy<Event>>() };
    let _ = proxy.send_event(Event::SourceInvalidated);
}
