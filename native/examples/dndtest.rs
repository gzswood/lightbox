//! 测试用：扮演 OLE 拖放**源**，把一个文件拖到指定屏幕坐标然后松手。
//!
//! 用来验证 LightBox 的放置目标（长条 / 弹出面板）是不是真的收下了文件。
//! 用法：
//!
//! ```text
//! cargo run --release --example dndtest -- <文件路径> <目标x> <目标y>
//! ```

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{implement, BOOL, Error, HRESULT, PCWSTR, Ref};
use windows::Win32::Foundation::{
    GlobalFree, HWND, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_FORMATETC, E_INVALIDARG, E_NOTIMPL, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl,
    IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, DVASPECT_CONTENT, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Ole::{
    DoDragDrop, DROPEFFECT, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropSource, IDropSource_Impl,
    OleInitialize, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MODIFIERKEYS_FLAGS, MK_LBUTTON, MK_RBUTTON};
use windows::Win32::UI::Input::KeyboardAndMouse::{mouse_event, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP};
use windows::Win32::UI::Shell::DROPFILES;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, SetCursorPos, SetForegroundWindow,
    SetProcessDPIAware, WNDCLASSW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const CF_HDROP: u16 = windows::Win32::System::Ole::CF_HDROP.0;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn preferred_dropeffect_id() -> u16 {
    static ID: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        let buf = wide("Preferred DropEffect");
        unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(buf.as_ptr())) as u16 }
    })
}

fn make_hdrop(files: &[PathBuf]) -> Option<windows::Win32::Foundation::HGLOBAL> {
    let mut names: Vec<u16> = Vec::new();
    for f in files {
        names.extend(f.as_os_str().encode_wide());
        names.push(0);
    }
    names.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let total = header + names.len() * 2;
    unsafe {
        let hg = GlobalAlloc(GHND, total).ok()?;
        let p = GlobalLock(hg);
        if p.is_null() {
            let _ = GlobalFree(Some(hg));
            return None;
        }
        let base = p as *mut u8;
        let df = &mut *(base as *mut DROPFILES);
        df.pFiles = header as u32;
        df.pt = Default::default();
        df.fNC = BOOL(0);
        df.fWide = BOOL(1);
        std::ptr::copy_nonoverlapping(
            names.as_ptr() as *const u8,
            base.add(header),
            names.len() * 2,
        );
        let _ = GlobalUnlock(hg);
        Some(hg)
    }
}

fn formats() -> Vec<FORMATETC> {
    vec![
        FORMATETC {
            cfFormat: CF_HDROP,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        },
        FORMATETC {
            cfFormat: preferred_dropeffect_id(),
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        },
    ]
}

#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
    formats: Vec<FORMATETC>,
    pos: AtomicUsize,
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        let start = self.pos.load(Ordering::Relaxed);
        let mut n = 0u32;
        for i in 0..celt as usize {
            let idx = start + i;
            if idx >= self.formats.len() {
                break;
            }
            unsafe { *rgelt.add(i) = self.formats[idx] };
            n += 1;
        }
        self.pos.store(start + n as usize, Ordering::Relaxed);
        if n == 0 {
            return S_FALSE;
        }
        // OLE 允许 pceltFetched 是空指针，写之前必须判空
        if !pceltfetched.is_null() {
            unsafe { *pceltfetched = n };
        }
        S_OK
    }
    fn Skip(&self, celt: u32) -> windows::core::Result<()> {
        let start = self.pos.load(Ordering::Relaxed);
        self.pos.store(start + celt as usize, Ordering::Relaxed);
        Ok(())
    }
    fn Reset(&self) -> windows::core::Result<()> {
        self.pos.store(0, Ordering::Relaxed);
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
        let e = FormatEnumerator {
            formats: self.formats.clone(),
            pos: AtomicUsize::new(0),
        };
        Ok(e.into())
    }
}

#[implement(IDataObject)]
struct FileListData {
    files: Vec<PathBuf>,
}

impl IDataObject_Impl for FileListData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let fe = unsafe { &*pformatetcin };
        if fe.cfFormat == CF_HDROP {
            let hg = make_hdrop(&self.files).ok_or(Error::from_hresult(E_INVALIDARG))?;
            return Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: hg },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }
        if fe.cfFormat == preferred_dropeffect_id() {
            let hg = unsafe { GlobalAlloc(GHND, 4) }.map_err(|_| Error::from_hresult(E_INVALIDARG))?;
            let p = unsafe { GlobalLock(hg) };
            if p.is_null() {
                let _ = unsafe { GlobalFree(Some(hg)) };
                return Err(Error::from_hresult(E_INVALIDARG));
            }
            unsafe {
                std::ptr::write_unaligned(p as *mut u32, DROPEFFECT_MOVE.0);
                let _ = GlobalUnlock(hg);
            }
            return Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: hg },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }
        Err(Error::from_hresult(E_INVALIDARG))
    }

    fn GetDataHere(&self, pformatetc: *const FORMATETC, pmedium: *mut STGMEDIUM) -> windows::core::Result<()> {
        let m = self.GetData(pformatetc)?;
        unsafe { *pmedium = m };
        Ok(())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        let fe = unsafe { &*pformatetc };
        if fe.cfFormat == CF_HDROP || fe.cfFormat == preferred_dropeffect_id() {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        unsafe { *pformatetcout = *pformatectin };
        S_OK
    }

    fn SetData(&self, _p: *const FORMATETC, _m: *const STGMEDIUM, _f: BOOL) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, _d: u32) -> windows::core::Result<IEnumFORMATETC> {
        let e = FormatEnumerator {
            formats: formats(),
            pos: AtomicUsize::new(0),
        };
        Ok(e.into())
    }

    fn DAdvise(&self, _p: *const FORMATETC, _a: u32, _s: Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn DUnadvise(&self, _d: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(Error::from_hresult(E_NOTIMPL))
    }
}

#[implement(IDropSource)]
struct FileDropSource;

impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        // 鼠标键已经松开 = 放手了，必须回 DRAGDROP_S_DROP，不然这次拖放不会结束
        if grfkeystate.0 & (MK_LBUTTON.0 | MK_RBUTTON.0) == 0 {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }
    fn GiveFeedback(&self, _e: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 造一个属于自己的小窗口当拖放源。
///
/// 关键点：鼠标按下必须落在**本进程**的窗口上，否则鼠标捕获在别的进程手里，
/// `DoDragDrop` 的模态循环收不到鼠标消息，既不会命中目标也不会结束。
fn source_window() -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let cls = wide("LightBoxDndTest");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_VISIBLE.0),
            20,
            20,
            160,
            100,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .ok()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("用法：dndtest <文件路径> <目标x> <目标y>");
        std::process::exit(2);
    }
    let file = PathBuf::from(&args[1]);
    let tx: i32 = args[2].parse().unwrap();
    let ty: i32 = args[3].parse().unwrap();

    unsafe {
        // 必须和被测程序一样按物理像素算坐标，否则 125% 缩放下会打偏
        let _ = SetProcessDPIAware();
        let _ = OleInitialize(None);
        let src = match source_window() {
            Some(h) => h,
            None => {
                eprintln!("造不出源窗口");
                std::process::exit(3);
            }
        };
        let _ = SetForegroundWindow(src);

        // 在源窗口上按下鼠标：这一步的捕获归属本进程，DoDragDrop 才收得到消息
        let _ = SetCursorPos(100, 70);
        std::thread::sleep(std::time::Duration::from_millis(150));
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        std::thread::sleep(std::time::Duration::from_millis(150));

        let data: IDataObject = FileListData {
            files: vec![file.clone()],
        }
        .into();
        let source: IDropSource = FileDropSource.into();
        let mut effect = DROPEFFECT_NONE;

        // 松手动作放到另一个线程：DoDragDrop 会进入自己的模态循环，
        // 主线程要忙着跑这个循环。
        std::thread::spawn(move || {
            // 分几步把鼠标挪到目标上，OLE 才会一路做命中测试（DragEnter/DragOver）
            let steps = 20;
            for i in 1..=steps {
                let x = 100 + (tx - 100) * i / steps;
                let y = 70 + (ty - 70) * i / steps;
                let _ = SetCursorPos(x, y);
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
        });

        let hr = DoDragDrop(&data, &source, DROPEFFECT_MOVE, &mut effect);
        println!(
            "DoDragDrop hr=0x{:08x} effect={:?}",
            hr.0,
            effect.0
        );
        let _ = ReleaseStgMedium;
    }
}
