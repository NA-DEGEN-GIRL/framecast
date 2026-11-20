//! "Browser page" capture: finds the web page viewport inside a browser window
//! (no tab strip, address bar, bookmarks bar or frame) and keeps tracking it.
//!
//! Chromium browsers (Chrome, Edge, Brave, Whale, Opera, Vivaldi…) host the page
//! in a child window of class `Chrome_RenderWidgetHostHWND`, which is cheap to
//! find and query. Others (Firefox and friends), or a Chromium build without that
//! child window, fall back to UI Automation: the page is the on-screen `Document`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW, GetWindowRect, IsWindowVisible};
use windows::core::BOOL;

use crate::capture::window_bounds;

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn area(r: &RECT) -> i64 {
    (r.right - r.left).max(0) as i64 * (r.bottom - r.top).max(0) as i64
}

fn intersect(a: &RECT, b: &RECT) -> RECT {
    RECT { left: a.left.max(b.left), top: a.top.max(b.top), right: a.right.min(b.right), bottom: a.bottom.min(b.bottom) }
}

/// `Chrome_RenderWidgetHostHWND` children: the largest visible one (the page;
/// background tabs are hidden), plus the rects of the hidden ones.
///
/// Chrome hides even the active tab's one while the window is covered by other
/// windows (its occlusion tracking), so then there is no visible one at all.
fn chromium_pages(hwnd: isize) -> (Option<RECT>, Vec<RECT>) {
    struct Found {
        visible: Option<RECT>,
        hidden: Vec<RECT>,
    }
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        unsafe {
            let f = &mut *(data.0 as *mut Found);
            let mut r = RECT::default();
            if class_name(h) == "Chrome_RenderWidgetHostHWND" && GetWindowRect(h, &mut r).is_ok() {
                if !IsWindowVisible(h).as_bool() {
                    f.hidden.push(r);
                } else if f.visible.is_none_or(|b| area(&r) > area(&b)) {
                    f.visible = Some(r);
                }
            }
        }
        BOOL(1)
    }
    let mut f = Found { visible: None, hidden: Vec::new() };
    unsafe {
        let _ = EnumChildWindows(Some(HWND(hwnd as _)), Some(cb), LPARAM(&mut f as *mut _ as isize));
    }
    (f.visible, f.hidden)
}

fn close(a: &RECT, b: &RECT) -> bool {
    let d = |x: i32, y: i32| (x - y).abs() <= 3;
    d(a.left, b.left) && d(a.top, b.top) && d(a.right, b.right) && d(a.bottom, b.bottom)
}

/// Top-level classes worth asking UI Automation about (it's slow on arbitrary apps).
fn uia_candidate(hwnd: isize) -> bool {
    matches!(class_name(HWND(hwnd as _)).as_str(), "MozillaWindowClass" | "Chrome_WidgetWin_1")
}

struct Uia {
    auto: IUIAutomation,
    cond: IUIAutomationCondition,
    el: Option<IUIAutomationElement>,
    found: Instant,
}

impl Uia {
    fn new() -> Option<Uia> {
        unsafe {
            let auto: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
            let doc = auto.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_DocumentControlTypeId.0)).ok()?;
            let onscreen = auto.CreatePropertyCondition(UIA_IsOffscreenPropertyId, &VARIANT::from(false)).ok()?;
            let cond = auto.CreateAndCondition(&doc, &onscreen).ok()?;
            Some(Uia { auto, cond, el: None, found: Instant::now() })
        }
    }

    fn page(&mut self, hwnd: isize) -> Option<RECT> {
        unsafe {
            // Re-find now and then: switching tabs swaps the document element.
            if self.el.is_none() || self.found.elapsed() > Duration::from_millis(1000) {
                let root = self.auto.ElementFromHandle(HWND(hwnd as _)).ok()?;
                self.el = root.FindFirst(TreeScope_Descendants, &self.cond).ok();
                self.found = Instant::now();
            }
            let r = self.el.as_ref()?.CurrentBoundingRectangle().ok();
            if r.is_none_or(|r| area(&r) == 0) {
                self.el = None;
                return None;
            }
            r
        }
    }
}

/// Locates the page viewport of one browser window. Keep one per window: it
/// caches the UI Automation element between queries.
pub struct PageFinder {
    hwnd: isize,
    uia: Option<Option<Uia>>,
}

impl PageFinder {
    pub fn new(hwnd: isize) -> PageFinder {
        PageFinder { hwnd, uia: None }
    }

    /// Screen rectangle of the web page (virtual-screen pixels), clipped to the window.
    /// Needs COM on the calling thread for the UI Automation fallback.
    pub fn rect(&mut self) -> Option<RECT> {
        let win = window_bounds(self.hwnd)?;
        let page = match chromium_pages(self.hwnd) {
            (Some(r), _) => r,
            (None, hidden) if uia_candidate(self.hwnd) => {
                // UI Automation bounds are rounded from DIPs: snap to the exact
                // window rect when a (hidden) render widget matches, else stay a
                // pixel inside so no toolbar row leaks into the video.
                let r = self.uia.get_or_insert_with(Uia::new).as_mut()?.page(self.hwnd)?;
                hidden.into_iter().find(|h| close(h, &r)).unwrap_or(RECT {
                    left: r.left + 1,
                    top: r.top + 1,
                    right: r.right - 1,
                    bottom: r.bottom - 1,
                })
            }
            (None, _) => return None,
        };
        let r = intersect(&page, &win);
        (r.right - r.left >= 64 && r.bottom - r.top >= 64).then_some(r)
    }
}

/// Where the page and the window currently are, in screen pixels.
#[derive(Clone, Copy, Debug)]
pub struct PageGeom {
    pub page: RECT,
    pub window: RECT,
}

/// Background thread that re-measures the page a few times a second, so the
/// crop follows window moves/resizes, bookmarks-bar toggles, DevTools, etc.
pub struct PageTracker {
    geom: Arc<Mutex<PageGeom>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PageTracker {
    pub fn start(hwnd: isize) -> Option<PageTracker> {
        let mut finder = PageFinder::new(hwnd);
        let first = PageGeom { page: finder.rect()?, window: window_bounds(hwnd)? };
        let geom = Arc::new(Mutex::new(first));
        let stop = Arc::new(AtomicBool::new(false));
        let (g, s) = (geom.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("page-tracker".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let mut finder = PageFinder::new(hwnd);
                while !s.load(Ordering::SeqCst) {
                    if let (Some(page), Some(window)) = (finder.rect(), window_bounds(hwnd)) {
                        *g.lock().unwrap() = PageGeom { page, window };
                    }
                    std::thread::park_timeout(Duration::from_millis(100));
                }
            })
            .ok()?;
        Some(PageTracker { geom, stop, thread: Some(thread) })
    }

    pub fn get(&self) -> PageGeom {
        *self.geom.lock().unwrap()
    }
}

impl Drop for PageTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
