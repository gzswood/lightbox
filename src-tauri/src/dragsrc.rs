//! 从收纳盒里把快捷方式「拖出」到桌面/资源管理器。
//!
//! 实现要点（踩过的坑都写在注释里）：
//! 1. 数据对象自己实现 `IDataObject`，只提供 `CF_HDROP` +
//!    `Preferred DropEffect = DROPEFFECT_MOVE`，让资源管理器按「移动」处理。
//! 2. 试过 `SHCreateDataObject(pidlFolder=NULL, 1, 绝对PIDL)`：返回成功，但对象对
//!    所有格式都答 S_FALSE，落点直接回 DROPEFFECT_NONE（拖过去没反应）。
//!    又试过 `SHBindToParent` + `IShellFolder::GetUIObjectOf`：在本机直接段错误。
//!    所以最后选了最朴素、最可控的 CF_HDROP。
//! 3. `DoDragDrop` 必须跑在拥有消息循环的主线程，并自己实现 `IDropSource`
//!    来决定何时「放下」。
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::win32::*;

type HRESULT = i32;

const MK_LBUTTON: u32 = 0x0001;
const MK_RBUTTON: u32 = 0x0002;

const CF_HDROP: u16 = 15;
const TYMED_HGLOBAL: u32 = 0x0001;
const DVASPECT_CONTENT: u32 = 0x1;
const GMEM_MOVEABLE: u32 = 0x0002;

const DV_E_FORMATETC: HRESULT = 0x8004_0064u32 as i32;
const DV_E_TYMED: HRESULT = 0x8004_0069u32 as i32;
const E_NOTIMPL: HRESULT = 0x8000_4001u32 as i32;
const OLE_E_ADVISENOTSUPPORTED: HRESULT = 0x8004_0003u32 as i32;

const PREFERRED_DROPEFFECT_NAME: &str = "Preferred DropEffect";

// ---------------------------------------------------------------- IDropSource

#[repr(C)]
struct DropSourceVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    query_continue_drag: unsafe extern "system" fn(*mut c_void, i32, u32) -> HRESULT,
    give_feedback: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
}

#[repr(C)]
struct DropSource {
    vtbl: *const DropSourceVtbl,
    refs: AtomicU32,
}

static QCD_COUNT: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn src_qi(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_NOINTERFACE;
    }
    if guid_eq(riid, &IID_IUNKNOWN) || guid_eq(riid, &IID_IDROPSOURCE) {
        *out = this;
        src_add_ref(this);
        S_OK
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn src_add_ref(this: *mut c_void) -> u32 {
    let s = &*(this as *const DropSource);
    s.refs.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn src_release(this: *mut c_void) -> u32 {
    let s = &*(this as *const DropSource);
    let left = s.refs.fetch_sub(1, Ordering::SeqCst) - 1;
    if left == 0 {
        drop(Box::from_raw(this as *mut DropSource));
    }
    left
}

/// OLE 每次鼠标移动/按键都会问：继续、放下、还是取消？
unsafe extern "system" fn query_continue_drag(
    _this: *mut c_void,
    escape_pressed: i32,
    key_state: u32,
) -> HRESULT {
    QCD_COUNT.fetch_add(1, Ordering::Relaxed);
    if escape_pressed != 0 {
        return DRAGDROP_S_CANCEL;
    }
    if key_state & (MK_LBUTTON | MK_RBUTTON) == 0 {
        return DRAGDROP_S_DROP;
    }
    S_OK
}

unsafe extern "system" fn give_feedback(_this: *mut c_void, _effect: u32) -> HRESULT {
    DRAGDROP_S_USEDEFAULTCURSORS
}

static SRC_VTBL: DropSourceVtbl = DropSourceVtbl {
    query_interface: src_qi,
    add_ref: src_add_ref,
    release: src_release,
    query_continue_drag,
    give_feedback,
};

// ------------------------------------------------------------- IEnumFORMATETC

#[repr(C)]
#[derive(Clone, Copy)]
struct FormatEtc {
    cf_format: u16,
    ptd: *mut c_void,
    dw_aspect: u32,
    lindex: i32,
    tymed: u32,
}

#[repr(C)]
struct StgMedium {
    tymed: u32,
    h_global: isize,
    p_unk_for_release: *mut c_void,
}


#[repr(C)]
struct EnumFormatEtcVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    next: unsafe extern "system" fn(*mut c_void, u32, *mut FormatEtc, *mut u32) -> HRESULT,
    skip: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    reset: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    clone: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct EnumFormatEtc {
    vtbl: *const EnumFormatEtcVtbl,
    refs: AtomicU32,
    formats: Vec<FormatEtc>,
    index: usize,
}

static S_FALSE_HR: HRESULT = 0x0000_0001;

unsafe extern "system" fn enum_qi(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_NOINTERFACE;
    }
    if guid_eq(riid, &IID_IUNKNOWN) || guid_eq(riid, &IID_IENUMFORMATETC) {
        *out = this;
        enum_add_ref(this);
        S_OK
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn enum_add_ref(this: *mut c_void) -> u32 {
    let s = &*(this as *const EnumFormatEtc);
    s.refs.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn enum_release(this: *mut c_void) -> u32 {
    let s = &*(this as *const EnumFormatEtc);
    let left = s.refs.fetch_sub(1, Ordering::SeqCst) - 1;
    if left == 0 {
        drop(Box::from_raw(this as *mut EnumFormatEtc));
    }
    left
}

unsafe extern "system" fn enum_next(
    this: *mut c_void,
    want: u32,
    out: *mut FormatEtc,
    fetched: *mut u32,
) -> HRESULT {
    let s = &mut *(this as *mut EnumFormatEtc);
    let mut n = 0u32;
    while n < want && s.index < s.formats.len() {
        if !out.is_null() {
            std::ptr::copy_nonoverlapping(&s.formats[s.index], out.add(n as usize), 1);
        }
        s.index += 1;
        n += 1;
    }
    if !fetched.is_null() {
        *fetched = n;
    }
    if n == want {
        S_OK
    } else {
        S_FALSE_HR
    }
}

unsafe extern "system" fn enum_skip(this: *mut c_void, count: u32) -> HRESULT {
    let s = &mut *(this as *mut EnumFormatEtc);
    s.index = (s.index + count as usize).min(s.formats.len());
    S_OK
}

unsafe extern "system" fn enum_reset(this: *mut c_void) -> HRESULT {
    (*(this as *mut EnumFormatEtc)).index = 0;
    S_OK
}

unsafe extern "system" fn enum_clone(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let s = &*(this as *const EnumFormatEtc);
    if out.is_null() {
        return E_NOTIMPL;
    }
    let copy = Box::new(EnumFormatEtc {
        vtbl: &ENUM_VTBL,
        refs: AtomicU32::new(1),
        formats: s.formats.clone(),
        index: s.index,
    });
    *out = Box::into_raw(copy) as *mut c_void;
    S_OK
}

static ENUM_VTBL: EnumFormatEtcVtbl = EnumFormatEtcVtbl {
    query_interface: enum_qi,
    add_ref: enum_add_ref,
    release: enum_release,
    next: enum_next,
    skip: enum_skip,
    reset: enum_reset,
    clone: enum_clone,
};

unsafe fn make_enum(formats: Vec<FormatEtc>) -> *mut c_void {
    Box::into_raw(Box::new(EnumFormatEtc {
        vtbl: &ENUM_VTBL,
        refs: AtomicU32::new(1),
        formats,
        index: 0,
    })) as *mut c_void
}

// --------------------------------------------------------------- IDataObject

#[repr(C)]
struct DataObjectVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_data: unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StgMedium) -> HRESULT,
    get_data_here: unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StgMedium) -> HRESULT,
    query_get_data: unsafe extern "system" fn(*mut c_void, *const FormatEtc) -> HRESULT,
    get_canonical_format_etc:
        unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut FormatEtc) -> HRESULT,
    set_data: unsafe extern "system" fn(
        *mut c_void,
        *const FormatEtc,
        *const StgMedium,
        i32,
    ) -> HRESULT,
    enum_format_etc: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    d_advise: unsafe extern "system" fn(
        *mut c_void,
        *const FormatEtc,
        u32,
        *mut c_void,
        *mut u32,
    ) -> HRESULT,
    d_unadvise: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    enum_d_advise: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct DataObject {
    vtbl: *const DataObjectVtbl,
    refs: AtomicU32,
    /// 待拖出的文件路径列表（每个都是宽字符、以 0 结尾；多选时一次拖多个）
    paths: Vec<Vec<u16>>,
    /// "Preferred DropEffect" 的剪贴板格式 id
    drop_effect_cf: u16,
}

unsafe extern "system" fn data_qi(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_NOINTERFACE;
    }
    if guid_eq(riid, &IID_IUNKNOWN) || guid_eq(riid, &IID_IDATAOBJECT) {
        *out = this;
        data_add_ref(this);
        S_OK
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn data_add_ref(this: *mut c_void) -> u32 {
    let s = &*(this as *const DataObject);
    s.refs.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn data_release(this: *mut c_void) -> u32 {
    let s = &*(this as *const DataObject);
    let left = s.refs.fetch_sub(1, Ordering::SeqCst) - 1;
    if left == 0 {
        drop(Box::from_raw(this as *mut DataObject));
    }
    left
}

/// DROPFILES 结构 + 宽字符文件列表（支持多个文件，末尾双 0 结尾），
/// 装进一块可移动的全局内存
unsafe fn make_hdrop(paths: &[Vec<u16>]) -> isize {
    #[repr(C)]
    struct DropFiles {
        p_files: u32,
        pt_x: i32,
        pt_y: i32,
        f_nc: i32,
        f_wide: i32,
    }
    let header = std::mem::size_of::<DropFiles>(); // 20
    // 每条路径「自身 + 一个分隔 0」，最后再补一个 0（双 0 收尾）。
    // `+1` 是给「调用方没带结尾 0」留的余量，多分配一点无所谓。
    let bytes = header + paths.iter().map(|p| (p.len() + 1) * 2).sum::<usize>() + 2;
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if h == 0 {
        return 0;
    }
    let p = GlobalLock(h) as *mut u8;
    if p.is_null() {
        GlobalFree(h);
        return 0;
    }
    let df = p as *mut DropFiles;
    (*df).p_files = header as u32;
    (*df).pt_x = 0;
    (*df).pt_y = 0;
    (*df).f_nc = 0;
    (*df).f_wide = 1;
    let mut list = p.add(header) as *mut u16;
    for path in paths {
        // 每条路径**只以单个 0 结尾**，多条之间用这个 0 分隔。
        //
        // 这里踩过一个很隐蔽的坑：`paths` 来自 `do_drag` 的
        // `encode_wide().chain(once(0))`，**已经带了结尾 0**。之前这里拷完再补一个 0，
        // 于是第一个文件后面紧跟的就是「空文件名」= 列表结束标记 ——
        // 资源管理器/桌面只认第一个文件，多选拖出永远只拖出去一个（用户实测）。
        let n = match path.last() {
            Some(&0) => path.len() - 1,
            _ => path.len(),
        };
        std::ptr::copy_nonoverlapping(path.as_ptr(), list, n);
        list = list.add(n);
        *list = 0;
        list = list.add(1);
    }
    *list = 0; // 末尾再补一个 0：与前一条的结尾 0 一起构成双 0 收尾
    GlobalUnlock(h);
    h
}

unsafe extern "system" fn get_data(
    this: *mut c_void,
    fmt: *const FormatEtc,
    medium: *mut StgMedium,
) -> HRESULT {
    if fmt.is_null() || medium.is_null() {
        return DV_E_FORMATETC;
    }
    let me = &*(this as *const DataObject);
    let f = &*fmt;
    if f.tymed & TYMED_HGLOBAL == 0 {
        return DV_E_TYMED;
    }

    if f.cf_format == CF_HDROP {
        let h = make_hdrop(&me.paths);
        if h == 0 {
            return E_NOTIMPL;
        }
        (*medium).tymed = TYMED_HGLOBAL;
        (*medium).h_global = h;
        (*medium).p_unk_for_release = std::ptr::null_mut();
        return S_OK;
    }

    if f.cf_format == me.drop_effect_cf {
        let h = GlobalAlloc(GMEM_MOVEABLE, 4);
        if h == 0 {
            return E_NOTIMPL;
        }
        let p = GlobalLock(h) as *mut u32;
        if !p.is_null() {
            // 告诉落点：优先按「移动」处理
            *p = DROPEFFECT_MOVE;
            GlobalUnlock(h);
        }
        (*medium).tymed = TYMED_HGLOBAL;
        (*medium).h_global = h;
        (*medium).p_unk_for_release = std::ptr::null_mut();
        return S_OK;
    }

    DV_E_FORMATETC
}

unsafe extern "system" fn get_data_here(
    _this: *mut c_void,
    _fmt: *const FormatEtc,
    _medium: *mut StgMedium,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn query_get_data(
    this: *mut c_void,
    fmt: *const FormatEtc,
) -> HRESULT {
    if fmt.is_null() {
        return DV_E_FORMATETC;
    }
    let me = &*(this as *const DataObject);
    let f = &*fmt;
    let ok_cf = f.cf_format == CF_HDROP || f.cf_format == me.drop_effect_cf;
    if ok_cf && f.dw_aspect & DVASPECT_CONTENT != 0 && f.tymed & TYMED_HGLOBAL != 0 {
        S_OK
    } else {
        0x0000_0001 // S_FALSE
    }
}

unsafe extern "system" fn get_canonical_format_etc(
    _this: *mut c_void,
    fmt_in: *const FormatEtc,
    fmt_out: *mut FormatEtc,
) -> HRESULT {
    if fmt_out.is_null() {
        return DV_E_FORMATETC;
    }
    // 表示「没有更规范的等价格式」，调用方可以直接用传进来的那个
    if !fmt_in.is_null() {
        std::ptr::copy_nonoverlapping(fmt_in, fmt_out, 1);
        (*fmt_out).ptd = std::ptr::null_mut();
    }
    0x0004_0130 // DATA_S_SAMEFORMATETC
}

unsafe extern "system" fn set_data(
    _this: *mut c_void,
    _fmt: *const FormatEtc,
    _medium: *const StgMedium,
    _release: i32,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn enum_format_etc(
    this: *mut c_void,
    direction: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    // DATADIR_GET = 1；set 方向不支持
    if direction != 1 || out.is_null() {
        return E_NOTIMPL;
    }
    let me = &*(this as *const DataObject);
    let formats = vec![
        FormatEtc {
            cf_format: CF_HDROP,
            ptd: std::ptr::null_mut(),
            dw_aspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        },
        FormatEtc {
            cf_format: me.drop_effect_cf,
            ptd: std::ptr::null_mut(),
            dw_aspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        },
    ];
    *out = make_enum(formats);
    S_OK
}

unsafe extern "system" fn d_advise(
    _this: *mut c_void,
    _fmt: *const FormatEtc,
    _advf: u32,
    _sink: *mut c_void,
    _conn: *mut u32,
) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn d_unadvise(_this: *mut c_void, _conn: u32) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn enum_d_advise(
    _this: *mut c_void,
    _out: *mut *mut c_void,
) -> HRESULT {
    E_NOTIMPL
}

static DATA_VTBL: DataObjectVtbl = DataObjectVtbl {
    query_interface: data_qi,
    add_ref: data_add_ref,
    release: data_release,
    get_data,
    get_data_here,
    query_get_data,
    get_canonical_format_etc,
    set_data,
    enum_format_etc,
    d_advise,
    d_unadvise,
    enum_d_advise,
};

unsafe fn com_release(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let vtbl = *(p as *mut *const *const c_void);
    let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
    f(p);
}

// ------------------------------------------------------------------- 对外接口

/// 开始一次拖拽。返回最终的 DROPEFFECT 位掩码（0 表示没被接受/取消）。
/// 把一批文件拖出去（多选时一次拖多个）。返回落点效果，见 DROPEFFECT_*。
pub fn do_drag(paths: &[PathBuf]) -> u32 {
    unsafe {
        OleInitialize(std::ptr::null_mut());
        QCD_COUNT.store(0, Ordering::Relaxed);

        let wide_paths: Vec<Vec<u16>> = paths
            .iter()
            .map(|p| p.as_os_str().encode_wide().chain(std::iter::once(0)).collect())
            .collect();

        let name = wide(PREFERRED_DROPEFFECT_NAME);
        let drop_effect_cf = RegisterClipboardFormatW(name.as_ptr()) as u16;

        let data = Box::into_raw(Box::new(DataObject {
            vtbl: &DATA_VTBL,
            refs: AtomicU32::new(1),
            paths: wide_paths,
            drop_effect_cf,
        }));
        let src = Box::into_raw(Box::new(DropSource {
            vtbl: &SRC_VTBL,
            refs: AtomicU32::new(1),
        }));

        let mut effect: u32 = DROPEFFECT_MOVE;
        let hr = DoDragDrop(
            data as *mut c_void,
            src as *mut c_void,
            DROPEFFECT_MOVE | DROPEFFECT_COPY | DROPEFFECT_LINK,
            &mut effect,
        );

        com_release(data as *mut c_void);
        com_release(src as *mut c_void);

        if hr < 0 {
            return 0;
        }
        effect
    }
}
