//! Windows 仅使用 UI Automation TextPattern，不模拟 Ctrl+C、不读取剪贴板取词。

use super::{Event, Origin};
use crate::core::{Failure, MAX_SELECTION_BYTES, Ticket};
use anyhow::{Result, bail};
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use tokio::runtime::Handle;
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HWND, POINT},
    System::{
        Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
    },
    UI::{
        Accessibility::{
            CUIAutomation8, HWINEVENTHOOK, IUIAutomation2, IUIAutomationTextPattern,
            SetWinEventHook, UIA_TextPatternId, UnhookWinEvent,
        },
        WindowsAndMessaging::{
            EVENT_OBJECT_DESTROY, EVENT_SYSTEM_FOREGROUND, GetCursorPos, GetForegroundWindow,
            GetWindowThreadProcessId, WINEVENT_OUTOFCONTEXT,
        },
    },
};
use winit::{
    event_loop::EventLoopProxy,
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::Window,
};

static EVENTS: OnceLock<EventLoopProxy<Event>> = OnceLock::new();
static SOURCE: AtomicUsize = AtomicUsize::new(0);

pub(super) struct Device {
    automation: IUIAutomation2,
    focus: HWINEVENTHOOK,
    closed: Option<HWINEVENTHOOK>,
    _apartment: Apartment,
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        // SAFETY: 创建者与 Drop 均为 Winit 主线程，成功初始化对应一次反初始化。
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

impl Device {
    pub const NAME: &'static str = "windows";
    pub fn new(proxy: EventLoopProxy<Event>, _: &Handle) -> Result<Self> {
        // SAFETY: COM 在拥有 Winit 消息循环的主线程初始化，并在该线程 Drop；回调仅投递事件。
        unsafe {
            let apartment = Apartment::new()?;
            let automation: IUIAutomation2 =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            automation.SetConnectionTimeout(200)?;
            automation.SetTransactionTimeout(200)?;
            EVENTS
                .set(proxy)
                .map_err(|_| anyhow::anyhow!("窗口事件入口重复初始化"))?;
            let focus = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(changed),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            if focus.0.is_null() {
                bail!("无法订阅 Windows 前台窗口事件")
            }
            Ok(Self {
                automation,
                focus,
                closed: None,
                _apartment: apartment,
            })
        }
    }
    pub fn check() -> Result<()> {
        // SAFETY: 不读取文本，不注册热键，不创建窗口；COM 生命周期在此作用域闭合。
        unsafe {
            let _apartment = Apartment::new()?;
            let _automation: IUIAutomation2 =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
        }
        println!("桌面：Windows；UI Automation 2 可创建；未读取选区、未注册快捷键、未映射窗口。");
        Ok(())
    }
    pub fn origin(&self) -> Result<Origin> {
        // SAFETY: 输出 POINT 由系统填充，HWND 仅作为身份，不解引用。
        unsafe {
            let mut point = POINT::default();
            GetCursorPos(&mut point)?;
            Ok(Origin {
                source: GetForegroundWindow().0 as u64,
                point: (point.x as f64, point.y as f64),
                logical: false,
            })
        }
    }
    pub fn capture(&mut self, ticket: Ticket) -> Result<()> {
        let origin = self.origin()?;
        let text = self.text(origin.source);
        let _ = EVENTS.get().unwrap().send_event(Event::Captured {
            ticket,
            origin,
            text,
        });
        Ok(())
    }
    fn text(&mut self, source: u64) -> std::result::Result<String, Failure> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
        let check = || {
            if std::time::Instant::now() < deadline {
                Ok(())
            } else {
                Err(Failure::CaptureTimeout)
            }
        };
        // SAFETY: 所有 UIA 接口由 COM 持有；GetText 的 UTF-16 长度有上界，读取前后校验窗口与焦点元素。
        unsafe {
            let window = HWND(source as usize as *mut _);
            let mut pid = 0;
            GetWindowThreadProcessId(window, Some(&mut pid));
            if source == 0 || pid == std::process::id() {
                return Err(Failure::CaptureUnavailable);
            }
            let element = self
                .automation
                .GetFocusedElement()
                .map_err(|_| Failure::PermissionRequired)?;
            if element
                .CurrentIsPassword()
                .map_err(|_| Failure::CaptureUnavailable)?
                .as_bool()
                || element
                    .CurrentProcessId()
                    .map_err(|_| Failure::CaptureUnavailable)? as u32
                    != pid
            {
                return Err(Failure::CaptureUnavailable);
            }
            check()?;
            let pattern: IUIAutomationTextPattern = element
                .GetCurrentPatternAs(UIA_TextPatternId)
                .map_err(|_| Failure::CaptureUnavailable)?;
            check()?;
            let ranges = pattern
                .GetSelection()
                .map_err(|_| Failure::CaptureUnavailable)?;
            let count = ranges.Length().map_err(|_| Failure::CaptureUnavailable)?;
            if !(0..=64).contains(&count) {
                return Err(Failure::SelectionTooLarge);
            }
            let mut source_text = String::new();
            for index in 0..count {
                check()?;
                let range = ranges
                    .GetElement(index)
                    .map_err(|_| Failure::CaptureUnavailable)?;
                check()?;
                let value = range
                    .GetText((MAX_SELECTION_BYTES + 1) as i32)
                    .map_err(|_| Failure::CaptureUnavailable)?;
                if value.len() > MAX_SELECTION_BYTES {
                    return Err(Failure::SelectionTooLarge);
                }
                let value = String::from_utf16(&value).map_err(|_| Failure::InvalidText)?;
                if !source_text.is_empty() && !value.is_empty() {
                    source_text.push('\n');
                }
                source_text.push_str(&value);
                if source_text.len() > MAX_SELECTION_BYTES {
                    return Err(Failure::SelectionTooLarge);
                }
            }
            check()?;
            let current = self
                .automation
                .GetFocusedElement()
                .map_err(|_| Failure::CaptureUnavailable)?;
            if GetForegroundWindow() != window
                || !self
                    .automation
                    .CompareElements(&element, &current)
                    .map_err(|_| Failure::CaptureUnavailable)?
                    .as_bool()
            {
                return Err(Failure::CaptureUnavailable);
            }
            check()?;
            if let Some(hook) = self.closed.take() {
                let _ = UnhookWinEvent(hook);
            }
            SOURCE.store(source as usize, Ordering::Relaxed);
            let hook = SetWinEventHook(
                EVENT_OBJECT_DESTROY,
                EVENT_OBJECT_DESTROY,
                None,
                Some(changed),
                pid,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            if hook.0.is_null() {
                return Err(Failure::CaptureUnavailable);
            }
            self.closed = Some(hook);
            Ok(source_text)
        }
    }
    pub fn cancel(&mut self) {
        SOURCE.store(0, Ordering::Relaxed);
        if let Some(hook) = self.closed.take() {
            unsafe {
                let _ = UnhookWinEvent(hook);
            }
        }
    }
    pub fn owns(&self, source: u64, _: &Window) -> bool {
        // SAFETY: HWND 为系统窗口标识，查询失败的 PID 保持为 0。
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(HWND(source as usize as *mut _), Some(&mut pid));
            pid == std::process::id()
        }
    }
    pub fn show(&self, window: &Window) -> Result<()> {
        use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, ShowWindow};
        let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
            bail!("不是 Windows 窗口")
        };
        // SAFETY: HWND 由存活的 Winit window 拥有，每次显示明确不激活。
        unsafe {
            let _ = ShowWindow(HWND(handle.hwnd.get() as *mut _), SW_SHOWNOACTIVATE);
        }
        Ok(())
    }
    pub fn hide(&self, window: &Window) {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(handle) = handle.as_raw()
        {
            unsafe {
                let _ = ShowWindow(HWND(handle.hwnd.get() as *mut _), SW_HIDE);
            }
        }
    }
    pub fn focus(&self, window: &Window) -> Result<()> {
        window.focus_window();
        Ok(())
    }
    pub fn copy(&mut self, text: String, window: &Window) -> Result<()> {
        let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
            bail!("不是 Windows 窗口")
        };
        let mut data: Vec<u16> = text.encode_utf16().collect();
        data.push(0);
        // SAFETY: 移动内存大小与 UTF-16 缓冲区一致；成功后所有权转交系统，失败时自行释放。
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2)?;
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                let _ = GlobalFree(Some(memory));
                bail!("无法分配剪贴板内存")
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), pointer.cast(), data.len());
            let _ = GlobalUnlock(memory);
            let result = (|| -> Result<()> {
                OpenClipboard(Some(HWND(handle.hwnd.get() as *mut _)))?;
                let result =
                    EmptyClipboard().and_then(|_| SetClipboardData(13, Some(HANDLE(memory.0))));
                let _ = CloseClipboard();
                result?;
                Ok(())
            })();
            if result.is_err() {
                let _ = GlobalFree(Some(memory));
            }
            result
        }
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: hooks 和 COM 均由此对象在同一消息循环线程创建。
        unsafe {
            let _ = UnhookWinEvent(self.focus);
            if let Some(hook) = self.closed.take() {
                let _ = UnhookWinEvent(hook);
            }
        }
    }
}

unsafe extern "system" fn changed(
    _: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if let Some(proxy) = EVENTS.get() {
        if event == EVENT_SYSTEM_FOREGROUND {
            let _ = proxy.send_event(Event::Focus(window.0 as u64));
        } else if event == EVENT_OBJECT_DESTROY
            && object == 0
            && window.0 as usize == SOURCE.load(Ordering::Relaxed)
        {
            let _ = proxy.send_event(Event::SourceInvalidated);
        }
    }
}
