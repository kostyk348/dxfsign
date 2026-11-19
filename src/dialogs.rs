//! dialogs.rs — системные диалоги Win7-класса: открыть файл и выбрать папку.
//!
//! Почему GetOpenFileName/SHBrowseForFolder, а не IFileDialog: `IFileOpenDialog`
//! требует COM-инициализации и появился позже; эти две функции есть на Win7
//! всегда, работают с UNC-путями и не тянут ни одной зависимости.
//!
//! Ограничение, названное прямо: многократный выбор (multi-select) не включён —
//! каталог и чертёж выбираются по одному, так проще не ошибиться на сетевом диске.

#![cfg(windows)]

use std::ffi::c_void;

#[repr(C)]
struct OpenFileNameA {
    l_struct_size: u32,
    hwnd_owner: isize,
    h_instance: isize,
    lpstr_filter: *const u8,
    lpstr_custom_filter: *const u8,
    n_max_cust_filter: u32,
    n_filter_index: u32,
    lpstr_file: *mut u8,
    n_max_file: u32,
    lpstr_file_title: *mut u8,
    n_max_file_title: u32,
    lpstr_initial_dir: *const u8,
    lpstr_title: *const u8,
    flags: u32,
    n_file_offset: u16,
    n_file_extension: u16,
    lpstr_def_ext: *const u8,
    lp_cust_data: isize,
    lpfn_hook: *const c_void,
    lp_template_name: *const u8,
    pv_reserved: *mut c_void,
    dw_reserved: u32,
    flags_ex: u32,
}

#[repr(C)]
struct BrowseInfoA {
    hwnd_owner: isize,
    pidl_root: *const c_void,
    psz_display_name: *mut u8,
    lpsz_title: *const u8,
    ul_flags: u32,
    lpfn: *const c_void,
    l_param: isize,
    i_image: i32,
}

#[link(name = "comdlg32")]
extern "system" {
    fn GetOpenFileNameA(ofn: *mut OpenFileNameA) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn SHBrowseForFolderA(bi: *mut BrowseInfoA) -> *mut c_void;
    fn SHGetPathFromIDListA(pidl: *const c_void, path: *mut u8) -> i32;
}

#[link(name = "ole32")]
extern "system" {
    fn CoTaskMemFree(p: *mut c_void);
}

const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
const OFN_EXPLORER: u32 = 0x0008_0000;
const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
const BIF_RETURNONLYFSDIRS: u32 = 0x0000_0001;
const BIF_NEWDIALOGSTYLE: u32 = 0x0000_0040;
const BIF_USENEWUI: u32 = BIF_NEWDIALOGSTYLE | 0x0000_0010;
const BIF_EDITBOX: u32 = 0x0000_0010;

/// Диалог «Открыть чертёж». Возвращает путь или None, если отменили.
/// O(1) по нашему коду: работа внутри системного диалога.
pub fn open_dxf_dialog(owner: isize, initial_dir: Option<&str>) -> Option<String> {
    // Фильтр — только ASCII (ANSI-диалог; «DXF (*.dxf)» понятно и без перевода).
    let filter = b"DXF (*.dxf)\0*.dxf\0All files (*.*)\0*.*\0\0";
    // Заголовок — в CP1251, потому что GetOpenFileNameA ждёт байты кодовой страницы.
    let mut title_v = Vec::with_capacity(32);
    crate::text::utf8_to_cp1251("Открыть чертёж".as_bytes(), &mut title_v);
    title_v.push(0);
    let def_ext = b"dxf\0";
    let mut buf = [0u8; 1024];
    let init = initial_dir.map(|s| {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    });
    let mut ofn = OpenFileNameA {
        l_struct_size: std::mem::size_of::<OpenFileNameA>() as u32,
        hwnd_owner: owner,
        h_instance: 0,
        lpstr_filter: filter.as_ptr(),
        lpstr_custom_filter: std::ptr::null(),
        n_max_cust_filter: 0,
        n_filter_index: 1,
        lpstr_file: buf.as_mut_ptr(),
        n_max_file: buf.len() as u32,
        lpstr_file_title: std::ptr::null_mut(),
        n_max_file_title: 0,
        lpstr_initial_dir: init.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()),
        lpstr_title: title_v.as_ptr(),
        flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_EXPLORER | OFN_NOCHANGEDIR,
        n_file_offset: 0,
        n_file_extension: 0,
        lpstr_def_ext: def_ext.as_ptr(),
        lp_cust_data: 0,
        lpfn_hook: std::ptr::null(),
        lp_template_name: std::ptr::null(),
        pv_reserved: std::ptr::null_mut(),
        dw_reserved: 0,
        flags_ex: 0,
    };
    let ok = unsafe { GetOpenFileNameA(&mut ofn) };
    if ok == 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).to_string())
}

/// Диалог «Выбрать папку» (для каталога сетевого диска).
pub fn pick_folder(owner: isize, title: &str) -> Option<String> {
    // SHBrowseForFolderA тоже ANSI: заголовок в CP1251.
    let mut t = Vec::with_capacity(title.len() + 4);
    crate::text::utf8_to_cp1251(title.as_bytes(), &mut t);
    t.push(0);
    let mut name = [0u8; 1024];
    let mut bi = BrowseInfoA {
        hwnd_owner: owner,
        pidl_root: std::ptr::null(),
        psz_display_name: name.as_mut_ptr(),
        lpsz_title: t.as_ptr(),
        ul_flags: BIF_RETURNONLYFSDIRS | BIF_USENEWUI | BIF_EDITBOX,
        lpfn: std::ptr::null(),
        l_param: 0,
        i_image: 0,
    };
    unsafe {
        let pidl = SHBrowseForFolderA(&mut bi);
        if pidl.is_null() {
            return None;
        }
        let mut path = [0u8; 1024];
        let ok = SHGetPathFromIDListA(pidl, path.as_mut_ptr());
        CoTaskMemFree(pidl);
        if ok == 0 {
            return None;
        }
        let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        Some(String::from_utf8_lossy(&path[..end]).to_string())
    }
}
