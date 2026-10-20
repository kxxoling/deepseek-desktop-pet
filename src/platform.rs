//! 平台相关能力。
//!
//! macOS 点击穿透：`NSWindow.setIgnoresMouseEvents:`（winit 无此 API）；
//! 窗口开启后收不到鼠标事件，恢复判定只能靠 `NSEvent.mouseLocation` 全局轮询。
//! Windows：透明走 `WS_EX_LAYERED` + `UpdateLayeredWindow`（swapchain 路径无 per-pixel
//! alpha），点击命中天然由位图 alpha 决定，无需轮询。Linux 未实现。

// objc2 桥接层：裸指针来自 winit 窗口句柄，存活期由 winit 保证
#![allow(unsafe_code)]

use winit::window::Window;

/// `AppKitWindowHandle` 暴露 `ns_view`，经 `NSView.window()` 取 `NSWindow`
#[cfg(target_os = "macos")]
fn ns_view_ptr(window: &Window) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(kit) => Some(kit.ns_view.as_ptr()),
        _ => None,
    }
}

/// 弹出菜单的常驻宿主窗口（1×1 透明 borderless，仅主线程使用）。
/// 以裸指针常驻：`Retained` 非 Sync，且窗口生命周期与进程相同，无需释放
#[cfg(target_os = "macos")]
fn popup_host(mtm: objc2_foundation::MainThreadMarker) -> &'static objc2_app_kit::NSWindow {
    use std::sync::OnceLock;
    static HOST: OnceLock<usize> = OnceLock::new();
    let &ptr = HOST.get_or_init(|| {
        use objc2_app_kit::{NSBackingStoreType, NSColor, NSWindow, NSWindowStyleMask};
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let host = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        host.setOpaque(false);
        host.setBackgroundColor(Some(&NSColor::clearColor()));
        let leaked = Box::leak(Box::new(host));
        std::ptr::from_ref(&**leaked) as usize
    });
    unsafe { &*(ptr as *const objc2_app_kit::NSWindow) }
}

#[cfg(target_os = "macos")]
fn with_ns_window<R>(window: &Window, f: impl FnOnce(&objc2_app_kit::NSWindow) -> R) -> Option<R> {
    let view: *mut objc2_app_kit::NSView = ns_view_ptr(window)?.cast();
    unsafe { (*view).window().map(|w| f(&w)) }
}

/// 设置窗口透明区域是否忽略鼠标事件（点击穿透）
pub fn set_ignore_cursor_events(window: &Window, ignore: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        with_ns_window(window, |w| w.setIgnoresMouseEvents(ignore)).is_some()
    }
    #[cfg(target_os = "windows")]
    {
        // ULW 窗口命中测试由位图 alpha 决定，无需手动切换
        let _ = (window, ignore);
        true
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (window, ignore);
        false
    }
}

/// 全局鼠标位置换算到窗口本地坐标（逻辑点，左上原点）。
/// 仅 macOS 需要（穿透切换后窗口收不到事件）；Windows ULW 命中由系统处理
pub fn global_mouse_local(window: &Window) -> Option<(f64, f64)> {
    #[cfg(target_os = "macos")]
    {
        with_ns_window(window, |w| {
            let frame = w.frame(); // 屏幕坐标（左下原点，逻辑点）
            let mouse = objc2_app_kit::NSEvent::mouseLocation();
            let local_x = mouse.x - frame.origin.x;
            let local_y_top = frame.size.height - (mouse.y - frame.origin.y);
            (local_x, local_y_top)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        None
    }
}

// ---------- Windows：分层窗口透明合成 ----------

#[cfg(target_os = "windows")]
pub mod windows_layered {
    //! `WS_EX_LAYERED` + `UpdateLayeredWindow`：把预乘 BGRA 位图交给系统合成器，
    //! 实现 swapchain 做不到的 per-pixel 透明；点击命中天然按位图 alpha。

    use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
    use windows::Win32::Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
        HGDIOBJ, SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, ULW_ALPHA, UpdateLayeredWindow,
        WS_EX_LAYERED,
    };

    /// 取窗口 HWND
    pub fn hwnd(window: &winit::window::Window) -> Option<HWND> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match window.window_handle().ok().map(|h| h.as_raw()) {
            Some(RawWindowHandle::Win32(k)) => Some(HWND(k.hwnd.get() as *mut _)),
            _ => None,
        }
    }

    /// 加 `WS_EX_LAYERED` 扩展样式（窗口创建后调用一次）
    pub fn make_layered(hwnd: HWND) -> bool {
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
            true
        }
    }

    /// 复用的 DIB 位图（物理像素级，top-down 32bpp）
    pub struct LayeredBitmap {
        hdc: HDC,
        hbmp: HBITMAP,
        /// DIB 像素内存（BGRA，长度 = `w*h*4`），由 GDI 管理
        pub pixels: *mut u8,
        w: i32,
        h: i32,
    }

    impl Drop for LayeredBitmap {
        fn drop(&mut self) {
            unsafe {
                // 先取消选择再删除，避免 DC 持有悬空位图
                SelectObject(self.hdc, HGDIOBJ::default());
                let _ = DeleteObject(HGDIOBJ(self.hbmp.0));
                let _ = DeleteDC(self.hdc);
            }
        }
    }

    impl LayeredBitmap {
        pub fn new(w: u32, h: u32) -> Option<Self> {
            if w == 0 || h == 0 {
                return None;
            }
            unsafe {
                let hdc = CreateCompatibleDC(None);
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w as i32,
                        biHeight: -(h as i32), // 负值 = top-down（屏幕坐标一致）
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut pixels: *mut std::ffi::c_void = std::ptr::null_mut();
                let hbmp = CreateDIBSection(
                    Some(hdc),
                    std::ptr::from_ref(&info),
                    DIB_RGB_COLORS,
                    std::ptr::from_mut(&mut pixels),
                    None,
                    0,
                )
                .ok()?;
                if pixels.is_null() {
                    return None;
                }
                let _ = SelectObject(hdc, HGDIOBJ(hbmp.0)); // 旧 stock bitmap 无需恢复
                Some(Self {
                    hdc,
                    hbmp,
                    pixels: pixels.cast::<u8>(),
                    w: w as i32,
                    h: h as i32,
                })
            }
        }

        /// 把回读的直通 alpha BGRA 预乘写入 DIB（ULW 要求 premultiplied）
        pub fn write_premultiplied(&mut self, src: &[u8], w: u32, h: u32) -> bool {
            if w as i32 != self.w || h as i32 != self.h || src.len() != (w * h * 4) as usize {
                return false;
            }
            unsafe {
                let dst = std::slice::from_raw_parts_mut(self.pixels, src.len());
                dst.copy_from_slice(src);
                let (chunks, _) = dst.as_chunks_mut::<4>();
                for px in chunks {
                    let a = u16::from(px[3]);
                    px[0] = (u16::from(px[0]) * a / 255) as u8;
                    px[1] = (u16::from(px[1]) * a / 255) as u8;
                    px[2] = (u16::from(px[2]) * a / 255) as u8;
                }
            }
            true
        }

        /// 提交给系统合成器（窗口内容整体替换；命中测试按位图 alpha）。
        /// `alpha` 为整体不透明度（0–255），乘进 `SourceConstantAlpha`
        pub fn present(&self, hwnd: HWND, alpha: u8) -> bool {
            unsafe {
                let size = SIZE {
                    cx: self.w,
                    cy: self.h,
                };
                let src_pt = POINT { x: 0, y: 0 };
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: alpha,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                UpdateLayeredWindow(
                    hwnd,
                    None, // hdcDst
                    None, // pptDst：窗口位置不变（由 winit 管理）
                    Some(std::ptr::from_ref(&size)),
                    Some(self.hdc),
                    Some(std::ptr::from_ref(&src_pt)),
                    COLORREF(0),
                    Some(std::ptr::from_ref(&blend)),
                    ULW_ALPHA,
                )
                .is_ok()
            }
        }
    }
}

/// 设置窗口整体不透明度（0.0–1.0）。
/// macOS 走 `NSWindow.alphaValue`，设置一次持续生效；Windows 在分层窗口提交时由 `SourceConstantAlpha` 承担（见 `LayeredBitmap::present`），这里无需做事；Linux 未实现。
pub fn set_window_opacity(window: &Window, opacity: f64) -> bool {
    #[cfg(target_os = "macos")]
    {
        with_ns_window(window, |w| w.setAlphaValue(opacity)).is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, opacity);
        true
    }
}

/// 检测当前是否有应用在全屏模式（macOS 判定依据：菜单栏隐藏时
/// `visibleFrame` 的高度约等于 `frame` 的高度；非全屏时菜单栏和 Dock 占几十像素）
pub fn any_app_fullscreen() -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::MainThreadMarker;
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let Some(screen) = objc2_app_kit::NSScreen::mainScreen(mtm) else {
            return false;
        };
        let frame = screen.frame();
        let visible = screen.visibleFrame();
        // 全屏时菜单栏和 Dock 都藏了，可见区域基本等于整块屏幕
        (frame.size.height - visible.size.height) < 4.0
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GWL_STYLE, GetForegroundWindow, GetWindowLongW, GetWindowRect, WS_CAPTION,
        };
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_invalid() {
                return false;
            }
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, std::ptr::from_mut(&mut rect)).is_err() {
                return false;
            }
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, std::ptr::from_mut(&mut info)).as_bool() {
                return false;
            }
            // 前台窗口盖满整块显示器
            let covers = rect.left <= info.rcMonitor.left
                && rect.top <= info.rcMonitor.top
                && rect.right >= info.rcMonitor.right
                && rect.bottom >= info.rcMonitor.bottom;
            // 且没有标题栏（有标题栏的是最大化，不算全屏——自动隐藏任务栏下的
            // 最大化窗口仍带标题栏，上游踩过的坑）
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            let has_caption = (style & WS_CAPTION.0 as i32) != 0;
            covers && !has_caption
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Linux 需要 X11 属性查询（_NET_WM_STATE_FULLSCREEN），当前无 X11 绑定依赖，先返回 false
        false
    }
}

/// 在窗口内指定位置（物理像素，左上原点）弹出上下文菜单
pub fn show_context_menu(menu: &muda::Menu, window: &Window, x: i32, y: i32) -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::MainThreadMarker;

        // 直接在 winit 宠物窗口的 view 上 popUp 会丢带子菜单的行（macOS 26 实测），
        // 换一个原生小窗口承载弹出。窗口常驻复用：反复创建并关闭承载窗口会偶发
        // AppKit 在渲染提交时过度释放窗口动画对象导致崩溃，弹出后只隐藏不关闭
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let host = popup_host(mtm);

        let scale = window.scale_factor();
        let Some(monitor) = window.current_monitor() else {
            return false;
        };
        let screen_h = f64::from(monitor.size().height) / monitor.scale_factor();
        let (ox, oy) = window.inner_position().map_or((0, 0), |p| (p.x, p.y));
        let px = f64::from(ox + x) / scale;
        let py = screen_h - f64::from(oy + y) / scale - 1.0;

        host.setFrameOrigin(objc2_foundation::NSPoint::new(px, py));
        host.orderFrontRegardless();
        let view = host.contentView().expect("弹出宿主窗口无 contentView");
        let shown = {
            use muda::ContextMenu;
            let ptr: *const std::ffi::c_void = std::ptr::from_ref(&*view).cast();
            unsafe {
                menu.show_context_menu_for_nsview(
                    ptr,
                    Some(muda::dpi::Position::Physical(
                        muda::dpi::PhysicalPosition::new(1, 1),
                    )),
                )
            }
        };
        host.orderOut(None);
        shown
    }
    #[cfg(target_os = "windows")]
    {
        use muda::ContextMenu;
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let pos = Some(muda::dpi::Position::Physical(
            muda::dpi::PhysicalPosition::new(x, y),
        ));
        match window.window_handle().ok().map(|h| h.as_raw()) {
            Some(RawWindowHandle::Win32(k)) => unsafe {
                menu.show_context_menu_for_hwnd(k.hwnd.get(), pos)
            },
            _ => false,
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        // X11/Wayland 上下文菜单暂不启用（见 README 平台说明）
        let _ = (menu, window, x, y);
        false
    }
}
