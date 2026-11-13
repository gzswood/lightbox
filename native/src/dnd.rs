//! OLE 拖放：把桌面图标**收进**盒子，把盒子里的图标**拖回**桌面。
//!
//! 两个方向都要走 OLE：
//! - **拖进来**（桌面 → 盒子）：窗口注册成 `IDropTarget`，收 `CF_HDROP`（文件路径列表）。
//! - **拖出去**（盒子 → 桌面）：面板提供 `IDataObject` + `IDropSource`，
//!   由桌面（Explorer）自己完成「移动文件」，我们只负责把数据递过去。
//!
//! 为什么不用 `WM_DROPFILES`：那是老 API，Explorer 现在一律走 OLE，
//! `DragAcceptFiles` 收不到任何东西。

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{implement, BOOL, Error, HRESULT, Ref};
use windows::Win32::Foundation::{
    GlobalFree, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC,
    E_NOTIMPL, POINTL, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl,
    IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, DVASPECT_CONTENT, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Ole::{
    DoDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
    IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MODIFIERKEYS_FLAGS, MK_LBUTTON, MK_RBUTTON};
use windows::Win32::UI::Shell::DROPFILES;

use crate::util::wide;

/// CF_HDROP：标准「文件列表」剪贴板格式。
///
/// 数值必须用系统常量（15），不能用 0x4000 —— 0x4000 落在「注册格式」的
/// 编号区间里，资源管理器根本不认这份数据，拖过去会被当成不支持的格式。
const CF_HDROP: u16 = windows::Win32::System::Ole::CF_HDROP.0;
/// "Preferred DropEffect"：告诉目标「我要移动，不是复制」
const PREFERRED_DROPEFFECT: &str = "Preferred DropEffect";

/// 「Preferred DropEffect」这种自定义格式要先注册才能拿到 id
fn preferred_dropeffect_id() -> u16 {
    static ID: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        let buf = wide(PREFERRED_DROPEFFECT);
        unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(buf.as_ptr())) as u16 }
    })
}

/// 构造 CF_HDROP 内存块：DROPFILES 头 + 双 0 结尾的文件名列表
fn make_hdrop(files: &[PathBuf]) -> Option<windows::Win32::Foundation::HGLOBAL> {
    let mut names: Vec<u16> = Vec::new();
    for f in files {
        names.extend(f.as_os_str().encode_wide());
        names.push(0);
    }
    names.push(0); // 列表结束标记

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

/// 解析 CF_HDROP 内存块 → 文件路径列表
fn parse_hdrop(hg: windows::Win32::Foundation::HGLOBAL) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let p = GlobalLock(hg);
        if p.is_null() {
            return out;
        }
        let base = p as *const u8;
        let df = &*(base as *const DROPFILES);
        let list = base.add(df.pFiles as usize);
        if df.fWide.as_bool() {
            let mut cur = list as *const u16;
            loop {
                let mut len = 0usize;
                while *cur.add(len) != 0 {
                    len += 1;
                }
                if len == 0 {
                    break;
                }
                out.push(String::from_utf16_lossy(std::slice::from_raw_parts(cur, len)));
                cur = cur.add(len + 1);
            }
        } else {
            let mut cur = list;
            loop {
                let mut len = 0usize;
                while *cur.add(len) != 0 {
                    len += 1;
                }
                if len == 0 {
                    break;
                }
                out.push(
                    String::from_utf8_lossy(std::slice::from_raw_parts(cur, len)).to_string(),
                );
                cur = cur.add(len + 1);
            }
        }
        let _ = GlobalUnlock(hg);
    }
    out
}

/// 支持的剪贴板格式
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

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// 格式枚举器
#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
    formats: Vec<FORMATETC>,
    pos: AtomicUsize,
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        // 调用方可以传 NULL 进来（COM 允许），不能直接解引用
        if rgelt.is_null() {
            return S_FALSE;
        }
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

/// 拖动时的数据对象：对外提供 CF_HDROP（文件路径列表）
#[implement(IDataObject)]
struct FileListData {
    files: Vec<PathBuf>,
}

impl IDataObject_Impl for FileListData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if pformatetcin.is_null() {
            return Err(Error::from_hresult(DV_E_FORMATETC));
        }
        let fe = unsafe { &*pformatetcin };
        if fe.cfFormat == CF_HDROP {
            let hg = make_hdrop(&self.files).ok_or(Error::from_hresult(E_NOTIMPL))?;
            return Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: hg },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }
        if fe.cfFormat == preferred_dropeffect_id() {
            // 一个 DWORD = DROPEFFECT_MOVE，桌面收到就知道是「移动」
            let hg = unsafe { GlobalAlloc(GHND, 4) }.map_err(|_| Error::from_hresult(E_NOTIMPL))?;
            let p = unsafe { GlobalLock(hg) };
            if p.is_null() {
                let _ = unsafe { GlobalFree(Some(hg)) };
                return Err(Error::from_hresult(E_NOTIMPL));
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
        Err(Error::from_hresult(DV_E_FORMATETC))
    }

    fn GetDataHere(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        if pmedium.is_null() {
            return Err(Error::from_hresult(DV_E_FORMATETC));
        }
        let m = self.GetData(pformatetc)?;
        unsafe { *pmedium = m };
        Ok(())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if pformatetc.is_null() {
            return DV_E_FORMATETC;
        }
        let fe = unsafe { &*pformatetc };
        if fe.cfFormat == CF_HDROP || fe.cfFormat == preferred_dropeffect_id() {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        if pformatectin.is_null() || pformatetcout.is_null() {
            return E_NOTIMPL;
        }
        let fe = unsafe { &*pformatectin };
        if fe.cfFormat != CF_HDROP && fe.cfFormat != preferred_dropeffect_id() {
            return E_NOTIMPL;
        }
        // 没有更规范的等价格式，直接把传进来的那个还回去
        unsafe {
            *pformatetcout = *pformatectin;
            (*pformatetcout).ptd = std::ptr::null_mut();
        }
        S_OK
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, _dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        let e = FormatEnumerator {
            formats: formats(),
            pos: AtomicUsize::new(0),
        };
        Ok(e.into())
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(Error::from_hresult(E_NOTIMPL))
    }
}

/// 拖动源：ESC 取消，松开左键就放下
#[implement(IDropSource)]
struct FileDropSource;

impl IDropSource_Impl for FileDropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        // 鼠标键已经松开 = 用户放手了，必须回 DRAGDROP_S_DROP。
        // 只回 S_OK 的话 OLE 会一直等下去，放下这一下永远不发生。
        if grfkeystate.0 & (MK_LBUTTON.0 | MK_RBUTTON.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// 拖放目标的身份：长条还是弹出面板
#[derive(Clone, Copy, Debug)]
pub enum DropMode {
    /// 长条：收进光标所在分段对应的盒子
    Bar,
    /// 弹出面板：收进面板当前打开的盒子
    Panel,
}

/// 放置目标：把拖进来的文件收进对应的盒子
#[implement(IDropTarget)]
struct BoxDropTarget {
    mode: DropMode,
}

impl BoxDropTarget_Impl {
    /// 光标落在哪个盒子上
    fn target_box(&self, pt: &POINTL) -> Option<String> {
        match self.mode {
            DropMode::Bar => crate::bar::box_id_at_screen(pt.x, pt.y),
            DropMode::Panel => crate::panel::current_box_id(),
        }
    }
}

impl IDropTarget_Impl for BoxDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let files = has_files(&pdataobj);
        let ok = files && self.target_box(pt).is_some();
        if !pdweffect.is_null() {
            unsafe { *pdweffect = if ok { DROPEFFECT_MOVE } else { DROPEFFECT_NONE } };
        }
        let _ = files; // 只用来算 effect，不写日志（拖拽过程每帧都触发，太吵）
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let target = self.target_box(pt);
        if !pdweffect.is_null() {
            unsafe { *pdweffect = if target.is_some() { DROPEFFECT_MOVE } else { DROPEFFECT_NONE } };
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let Some(box_id) = self.target_box(pt) else {
            crate::log::info(&format!("Drop 落点无效 ({},{})", pt.x, pt.y));
            if !pdweffect.is_null() {
                unsafe { *pdweffect = DROPEFFECT_NONE };
            }
            return Ok(());
        };
        let files = extract_files(&pdataobj);
        crate::log::info(&format!(
            "Drop 收进「{box_id}」：{} 个文件",
            files.len()
        ));
        if files.is_empty() {
            if !pdweffect.is_null() {
                unsafe { *pdweffect = DROPEFFECT_NONE };
            }
            return Ok(());
        }
        if !pdweffect.is_null() {
            unsafe { *pdweffect = DROPEFFECT_MOVE };
        }
        import_into(&box_id, &files);
        Ok(())
    }
}

/// 数据对象里有没有 CF_HDROP
fn has_files(obj: &Ref<IDataObject>) -> bool {
    let data = match obj.as_ref() {
        Some(d) => d,
        None => return false,
    };
    let fe = hdrop_format();
    unsafe { data.QueryGetData(&fe).is_ok() }
}

/// 从数据对象里取出文件路径（优先 CF_HDROP）
fn extract_files(obj: &Ref<IDataObject>) -> Vec<String> {
    let data = match obj.as_ref() {
        Some(d) => d,
        None => return Vec::new(),
    };
    let fe = hdrop_format();
    let mut medium = match unsafe { data.GetData(&fe) } {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    let mut files = Vec::new();
    unsafe {
        let hg = medium.u.hGlobal;
        if !hg.is_invalid() {
            files = parse_hdrop(hg);
        }
        ReleaseStgMedium(&mut medium);
    }
    files
}

/// 把文件收进盒子：真的搬进盒子目录，然后刷新长条和面板
fn import_into(box_id: &str, files: &[String]) {
    let (added, skipped) = crate::config::import(box_id, files);
    for s in &skipped {
        crate::log::info(&format!("跳过：{s}"));
    }
    if !added.is_empty() {
        crate::log::info(&format!("收纳 {} 个图标进盒子", added.len()));
    }
    crate::bar::refresh();
    crate::panel::reload();
}

/// 把窗口注册成放置目标。返回的 `IDropTarget` 要一直留着，
/// 窗口销毁前得先 `RevokeDragDrop`。
pub fn register_drop_target(
    hwnd: windows::Win32::Foundation::HWND,
    mode: DropMode,
) -> Option<IDropTarget> {
    let target: IDropTarget = BoxDropTarget { mode }.into();
    let r = unsafe { RegisterDragDrop(hwnd, &target) };
    match r {
        Ok(()) => {
            Some(target)
        }
        Err(e) => {
            crate::log::info(&format!("注册放置目标失败（{:?}）：{e}", mode));
            None
        }
    }
}

/// 取消放置目标
pub fn unregister_drop_target(hwnd: windows::Win32::Foundation::HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

/// 发起一次「把文件拖出去」的 OLE 拖动。
/// 返回值是最终效果（`DROPEFFECT_MOVE` 表示目标把它移走了）。
pub fn start_drag_out(files: Vec<PathBuf>) -> DROPEFFECT {
    if files.is_empty() {
        return DROPEFFECT_NONE;
    }
    let data: IDataObject = FileListData { files }.into();
    let source: IDropSource = FileDropSource.into();
    let mut effect = DROPEFFECT_NONE;
    unsafe {
        // 允许的效果给全一点：有些落点（比如桌面）只认 COPY，
        // 具体是移动还是复制由 "Preferred DropEffect" 决定。
        let hr = DoDragDrop(
            &data,
            &source,
            DROPEFFECT_MOVE | DROPEFFECT_COPY | DROPEFFECT_LINK,
            &mut effect,
        );
        if hr.is_err() {
            crate::log::info(&format!("拖出失败：hr={hr:?}"));
        }
    }
    effect
}
