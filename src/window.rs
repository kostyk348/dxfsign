//! window.rs — оконный режим на чистом Win32 (Win7-совместимо: user32+gdi32).
//!
//! Что делает окно:
//!   * рисует кадр из `render::Renderer` в окно (кадр собирается в память, затем
//!     копируется в DIB и отдаётся StretchDIBits — никакого DirectX/Direct2D,
//!     которые на Win7 требуют другого рантайма);
//!   * выбор надписи кликом (hit-test по спроецированному боксу: O(T) по надписям);
//!   * F2 — правка выбранной надписи, F3 — только комментарий;
//!   * модальный шаг: ТЕКСТ -> НОМЕР ПРОВОДА (обязателен) -> КОММЕНТАРИЙ;
//!     Tab переключает поле, Enter принимает, Esc отменяет правку целиком;
//!   * Ctrl+S — сохранение по протоколу версий (старый файл -> `_VERSIONS`,
//!     рабочий перезаписывается, `.txt`, `.eml`, якорь цепочки);
//!   * +/- масштаб, колесо мыши — масштаб, средняя кнопка — панорама.
//!
//! Границы, названные прямо: IME (ввод через китайскую/японскую раскладку) не
//! поддержан, буфер обмена не подключён, окно не переживает DPI-aware масштаб
//! 150%+ без размытия (SetProcessDPIAware вызывается, но кадр рисуется 1:1).

#![cfg(windows)]

use std::ffi::c_void;

use crate::config::AppConfig;
use crate::dialogs;
use crate::doc::Doc;
use crate::edit::{EditSession, Outcome, Step, WireError};
use crate::render::{Camera, Renderer, TextPainter, UiState};
use crate::win32::GdiPainter;

type Hwnd = isize;
type Hdc = isize;
type Hinstance = isize;
type Hbitmap = isize;
type Hgdiobj = isize;
type Hmenu = isize;
type Hbrush = isize;
type Hicon = isize;
type Hcursor = isize;

#[repr(C)]
struct WndClassA {
    style: u32,
    lpfn_wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: Hinstance,
    h_icon: Hicon,
    h_cursor: Hcursor,
    hbr_background: Hbrush,
    lpsz_menu_name: *const u8,
    lpsz_class_name: *const u8,
}

#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Default)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: Point,
    l_private: u32,
}

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Default)]
struct PaintStruct {
    hdc: Hdc,
    f_erase: i32,
    _pad: i32,
    rc_paint: Rect,
    f_restore: i32,
    f_inc_update: i32,
    rgb_reserved: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassA(wc: *const WndClassA) -> u16;
    fn CreateWindowExA(
        ex_style: u32,
        class_name: *const u8,
        window_name: *const u8,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Hwnd,
        menu: Hmenu,
        instance: Hinstance,
        param: *mut c_void,
    ) -> Hwnd;
    fn DefWindowProcA(hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize;
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    fn UpdateWindow(hwnd: Hwnd) -> i32;
    fn GetMessageA(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageA(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn BeginPaint(hwnd: Hwnd, ps: *mut PaintStruct) -> Hdc;
    fn EndPaint(hwnd: Hwnd, ps: *const PaintStruct) -> i32;
    fn GetClientRect(hwnd: Hwnd, r: *mut Rect) -> i32;
    fn InvalidateRect(hwnd: Hwnd, r: *const Rect, erase: i32) -> i32;
    fn SetTimer(hwnd: Hwnd, id: usize, ms: u32, proc: *const c_void) -> usize;
    fn GetKeyState(vk: i32) -> i16;
    fn SetProcessDPIAware() -> i32;
    fn LoadCursorA(inst: Hinstance, name: *const u8) -> Hcursor;
    fn SetWindowTextA(hwnd: Hwnd, s: *const u8) -> i32;
    fn GetModuleHandleA(name: *const u8) -> Hinstance;
}

#[link(name = "kernel32")]
extern "system" {
    fn FreeConsole() -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn SetCapture(hwnd: Hwnd) -> Hwnd;
    fn ReleaseCapture() -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
    fn DeleteDC(hdc: Hdc) -> i32;
    fn CreateDIBSection(
        hdc: Hdc,
        pbmi: *const BitmapInfoHeader,
        usage: u32,
        ppv_bits: *mut *mut c_void,
        h_section: isize,
        offset: u32,
    ) -> Hbitmap;
    fn SelectObject(hdc: Hdc, obj: Hgdiobj) -> Hgdiobj;
    fn DeleteObject(obj: Hgdiobj) -> i32;
    fn StretchDIBits(
        hdc: Hdc,
        x_dest: i32,
        y_dest: i32,
        w_dest: i32,
        h_dest: i32,
        x_src: i32,
        y_src: i32,
        w_src: i32,
        h_src: i32,
        bits: *const c_void,
        pbmi: *const BitmapInfoHeader,
        usage: u32,
        rop: u32,
    ) -> i32;
}

const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const CW_USEDEFAULT: i32 = -2147483648;
const SW_SHOW: i32 = 5;
const WM_PAINT: u32 = 0x000F;
const WM_DESTROY: u32 = 0x0002;
const WM_KEYDOWN: u32 = 0x0100;
const WM_CHAR: u32 = 0x0102;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_CAPTURECHANGED: u32 = 0x0215;
const WM_TIMER: u32 = 0x0113;
const WM_CLOSE: u32 = 0x0010;
const WM_GETMINMAXINFO: u32 = 0x0024;
const VK_BACK: usize = 0x08;
const VK_TAB: usize = 0x09;
const VK_RETURN: usize = 0x0D;
const VK_ESCAPE: usize = 0x1B;
const VK_CONTROL: i32 = 0x11;
const VK_F2: usize = 0x71;
const VK_F3: usize = 0x72;
const VK_F4: usize = 0x73;
const VK_F5: usize = 0x74;
const VK_F7: usize = 0x76;
const VK_INSERT: usize = 0x2D;
const VK_DELETE: usize = 0x2E;
const VK_UP: usize = 0x26;
const VK_DOWN: usize = 0x28;
const VK_O: usize = 0x4F;
const VK_ADD: usize = 0x6B;
const VK_SUBTRACT: usize = 0x6D;
const VK_OEM_PLUS: usize = 0xBB;
const VK_OEM_MINUS: usize = 0xBD;
const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00CC_0020;
const SW_SHOWMAXIMIZED: i32 = 3;

/// Что должно произойти после нажатия Ctrl+S — решает вызывающий (main.rs),
/// окно не знает, где конфиг и куда кладётся спул.
pub type SaveHook = fn(&Doc, &AppConfig) -> String;

/// Редактор каталога сетевого диска: список записей из ini + два поля.
/// Всё на фиксированных буферах — кадр не аллоцирует.
struct CatEditor {
    open: bool,
    sel: usize,
    name: crate::text::EditBuffer,
    path: crate::text::EditBuffer,
    focus: u8,
    note: [u8; 120],
    note_len: usize,
}

impl CatEditor {
    const fn new() -> Self {
        Self {
            open: false,
            sel: 0,
            name: crate::text::EditBuffer::new(),
            path: crate::text::EditBuffer::new(),
            focus: 0,
            note: [0; 120],
            note_len: 0,
        }
    }
    fn note(&mut self, s: &str) {
        let b = s.as_bytes();
        let n = b.len().min(self.note.len());
        self.note[..n].copy_from_slice(&b[..n]);
        self.note_len = n;
    }
    fn note_str(&self) -> &str {
        std::str::from_utf8(&self.note[..self.note_len]).unwrap_or("")
    }
}

struct Ctx {
    cfg: AppConfig,
    cfg_path: String,
    editor: CatEditor,
    doc: Doc,
    session: EditSession,
    ui: UiState,
    renderer: Renderer,
    painter: GdiPainter,
    cam: Camera,
    /// Центр вида в мировых координатах и масштаб — единственный источник правды
    /// для камеры. Раньше кадр каждый раз пересчитывал камеру от границ чертежа,
    /// поэтому панорама и зум сбрасывались.
    view_center: (f64, f64),
    zoom: f64,
    panning: bool,
    pan_from: (i32, i32),
    hwnd: Hwnd,
    mem_dc: Hdc,
    bmp: Hbitmap,
    old_bmp: Hgdiobj,
    bits: *mut u32,
    dib_w: i32,
    dib_h: i32,
    /// 0 — текст, 1 — провод, 2 — комментарий
    focus: u8,
    save_hook: Option<SaveHook>,
    /// Строка статуса для окна, чтобы человек видел ответ на действие.
    flash: [u8; 160],
    flash_len: usize,
    last_click_text: Option<usize>,
    last_click_ms: u64,
}

static mut CTX: *mut Ctx = std::ptr::null_mut();

fn ctx() -> &'static mut Ctx {
    unsafe { &mut *CTX }
}

fn set_flash(s: &str) {
    let c = ctx();
    let b = s.as_bytes();
    let n = b.len().min(c.flash.len());
    c.flash[..n].copy_from_slice(&b[..n]);
    c.flash_len = n;
    c.ui.set_status(s);
}

fn ensure_dib(w: i32, h: i32) {
    let c = ctx();
    if c.bits.is_null() || c.dib_w != w || c.dib_h != h {
        unsafe {
            if !c.bmp == 0 {
                SelectObject(c.mem_dc, c.old_bmp);
                DeleteObject(c.bmp);
            }
            let bih = BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                bi_width: w,
                bi_height: -h, // top-down: строка 0 сверху, копирование по порядку
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: 0,
                bi_size_image: (w * h * 4) as u32,
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(c.mem_dc, &bih, DIB_RGB_COLORS, &mut bits, 0, 0);
            if bmp != 0 {
                c.old_bmp = SelectObject(c.mem_dc, bmp);
                c.bmp = bmp;
                c.bits = bits as *mut u32;
                c.dib_w = w;
                c.dib_h = h;
            }
        }
    }
}

fn render_and_present() {
    let c = ctx();
    let mut r = Rect::default();
    unsafe {
        GetClientRect(c.hwnd, &mut r);
    }
    let w = (r.right - r.left).max(320);
    let h = (r.bottom - r.top).max(240);
    c.renderer.fb.resize(w as usize, h as usize);
    let view_w = (w - crate::render::swiss::LEFT_W - crate::render::swiss::RIGHT_W).max(32);
    let view_h = (h - crate::render::swiss::TOP_H - crate::render::swiss::STATUS_H).max(32);
    if c.zoom <= 0.0 {
        let fit = Camera::fit(c.doc.bounds(), view_w, view_h, 64);
        c.zoom = fit.scale;
        let (x0, y0, x1, y1) = c.doc.bounds();
        c.view_center = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    }
    c.cam = Camera {
        scale: c.zoom,
        ox: view_w as f64 * 0.5 - c.view_center.0 * c.zoom,
        oy: view_h as f64 * 0.5 + c.view_center.1 * c.zoom,
    };
    c.ui.focus = c.focus;
    let session = &c.session;
    let _ = c.renderer.frame(&c.doc, c.cam, &mut c.ui, session, &mut c.painter);
    if c.editor.open {
        let names = String::from_utf8_lossy(c.editor.name.as_bytes()).to_string();
        let paths = String::from_utf8_lossy(c.editor.path.as_bytes()).to_string();
        let mut head = String::new();
        head.push_str(&format!(
            "записей: {} · выбрана: {}",
            c.cfg.catalog.len(),
            c.editor.sel + 1
        ));
        let note = c.editor.note_str().to_string();
        let note2 = note.clone();
        let hint = String::from_utf8_lossy(note2.as_bytes()).to_string();
        let fields = [
            ("ИМЯ В КАТАЛОГЕ", names.as_str(), c.editor.focus == 0),
            ("ПУТЬ НА СЕТЕВОМ ДИСКЕ", paths.as_str(), c.editor.focus == 1),
        ];
        let mut painter = std::mem::replace(&mut c.painter, GdiPainter::new().unwrap());
        c.renderer.draw_form(&mut painter, "КАТАЛОГ СЕТЕВОГО ДИСКА", &head, &fields, &hint);
        c.painter = painter;
    }
    ensure_dib(w, h);
    if c.bits.is_null() {
        return;
    }
    // пиксели кадра -> DIB с обменом R и B (BMP/DIB ждёт BGRA)
    let fb = &c.renderer.fb;
    unsafe {
        for i in 0..(w as usize * h as usize).min(fb.px.len()) {
            let p = fb.px[i];
            let b = p & 0xFF;
            let g = (p >> 8) & 0xFF;
            let r = (p >> 16) & 0xFF;
            *c.bits.add(i) = (r << 16) | (g << 8) | b;
        }
        let bih = BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: w,
            bi_height: -h,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: 0,
            bi_size_image: (w * h * 4) as u32,
            ..Default::default()
        };
        let hdc = BeginPaint(c.hwnd, &mut PaintStruct::default());
        StretchDIBits(
            hdc, 0, 0, w, h, 0, 0, w, h, c.bits as *const c_void, &bih, DIB_RGB_COLORS, SRCCOPY,
        );
        EndPaint(c.hwnd, &PaintStruct::default());
    }
}

/// Выбор надписи по клику: бокс вокруг спроецированной точки,
/// размер — из высоты текста и метрики шрифта. O(T) по надписям.
fn hit_test_text(mx: i32, my: i32) -> Option<usize> {
    let c = ctx();
    let vx = crate::render::swiss::LEFT_W;
    let vy = crate::render::swiss::TOP_H;
    let mut best: Option<(usize, i32)> = None;
    for (i, t) in c.doc.texts.iter().enumerate() {
        let pi = t.pos as usize;
        if pi >= c.renderer.sx.len() {
            continue;
        }
        let x = vx + c.renderer.sx[pi];
        let y = vy + c.renderer.sy[pi];
        let px = ((t.height * c.cam.scale) as i32).clamp(13, 72);
        let s = String::from_utf8_lossy(c.doc.arena_get(t.s)).to_string();
        let w = c.painter.measure(px, false, &s).max(px);
        // запас 14 px по краям: попасть в строку высотой 6 единиц одним пикселем
        // было физически тяжело — мышью «ничего не кликалось»
        const SLOP: i32 = 14;
        let (bx0, by0, bx1, by1) = (x - SLOP, y - px - SLOP, x + w + SLOP, y + SLOP);
        if mx >= bx0 && mx <= bx1 && my >= by0 && my <= by1 {
            let d = (mx - (bx0 + bx1) / 2).abs() + (my - (by0 + by1) / 2).abs();
            if best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((i, d));
            }
        }
    }
    best.map(|(i, _)| i)
}

fn current_field_buf() -> &'static mut crate::text::EditBuffer {
    let c = ctx();
    match c.focus {
        1 => &mut c.session.wire,
        2 => &mut c.session.comment,
        _ => &mut c.session.text,
    }
}

fn start_edit(comment_only: bool) {
    let c = ctx();
    let Some(idx) = c.ui.selected_text else {
        set_flash("Сначала выберите надпись на чертеже (клик по тексту)");
        return;
    };
    let cur = c.doc.arena_get(c.doc.texts[idx].s).to_vec();
    c.session.begin(idx, &cur);
    if comment_only {
        c.session.text.set(&cur);
        c.session.comment.set(b"");
        set_flash("F3: комментарий. Tab — поле, Enter — принять, Esc — отмена");
    } else {
        set_flash("F2: правка текста. Tab — поле, Enter — принять, Esc — отмена");
    }
    c.focus = if comment_only { 2 } else { 0 };
}

/// Вписать чертёж в окно (центр + масштаб). O(1) по границам.
/// Одно место для действий тулбара: код -> вызов. Кнопки и клавиши ходят сюда же.
fn run_tool(code: u8) {
    let c = ctx();
    match code {
        1 => {
            let start = save_dir_of(&c.doc.path);
            if let Some(path) = dialogs::open_dxf_dialog(c.hwnd, start.as_deref()) {
                open_path(&path);
            } else {
                set_flash("Открытие отменено");
            }
        }
        2 => {
            c.editor.open = true;
            c.editor.sel = 0;
            load_editor_from_cfg();
            c.editor.note("Ins добавить · Del удалить · F5 выбрать папку · Enter сохранить");
        }
        3 => start_edit(false),
        4 => start_edit(true),
        5 => {
            if c.session.is_busy() {
                set_flash("Сначала примите правку (Enter) или отмените (Esc)");
            } else if c.doc.log.is_empty() {
                set_flash("Нет правок в этой сессии — сохранять нечего");
            } else {
                do_save();
            }
        }
        6 => fit_view(),
        7 => {
            c.ui.help_open = !c.ui.help_open;
            set_flash(if c.ui.help_open { "Справка: Esc или кнопка «СОХРАНИТЬ» закрывает" } else { "Справка закрыта" });
        }
        _ => {}
    }
}

fn fit_view() {
    let c = ctx();
    let view_w = (c.renderer.fb.w as i32 - crate::render::swiss::LEFT_W - crate::render::swiss::RIGHT_W).max(32);
    let view_h = (c.renderer.fb.h as i32 - crate::render::swiss::TOP_H - crate::render::swiss::STATUS_H).max(32);
    let fit = Camera::fit(c.doc.bounds(), view_w, view_h, 64);
    c.zoom = fit.scale;
    let (x0, y0, x1, y1) = c.doc.bounds();
    c.view_center = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    set_flash("Чертёж вписан в окно");
}

/// Зум вокруг точки экрана: мировая точка под курсором остаётся на месте.
/// O(1). Иначе зум «прыгает» и человек теряет место, где работал.
fn zoom_at(screen_x: i32, screen_y: i32, factor: f64) {
    let c = ctx();
    let view_w = (c.renderer.fb.w as i32 - crate::render::swiss::LEFT_W - crate::render::swiss::RIGHT_W).max(32);
    let view_h = (c.renderer.fb.h as i32 - crate::render::swiss::TOP_H - crate::render::swiss::STATUS_H).max(32);
    let lx = (screen_x - crate::render::swiss::LEFT_W) as f64;
    let ly = (screen_y - crate::render::swiss::TOP_H) as f64;
    let s = c.zoom.max(1e-9);
    // мировая точка под курсором до зума
    let wx = (lx - view_w as f64 * 0.5) / s + c.view_center.0;
    let wy = (view_h as f64 * 0.5 - ly) / s + c.view_center.1;
    let s2 = (s * factor).clamp(0.005, 400.0);
    // после зума держим ту же точку под тем же пикселем
    c.view_center.0 = wx - (lx - view_w as f64 * 0.5) / s2;
    c.view_center.1 = wy - (view_h as f64 * 0.5 - ly) / s2;
    c.zoom = s2;
}

/// Панорама мышью в пикселях: сдвигаем центр вида. O(1).
fn pan_by(dx: i32, dy: i32) {
    let c = ctx();
    let s = c.zoom.max(1e-9);
    c.view_center.0 -= dx as f64 / s;
    c.view_center.1 += dy as f64 / s;
}

fn scroll_texts(delta: i32) {
    let c = ctx();
    let rows = ((c.renderer.fb.h as i32 - crate::render::swiss::TOP_H - crate::render::swiss::STATUS_H)
        / crate::render::swiss::ROW_H)
        .max(1) as usize;
    let max_scroll = c.doc.texts.len().saturating_sub(rows.min(c.doc.texts.len()));
    let cur = c.ui.texts_scroll as i32 + delta;
    c.ui.texts_scroll = cur.clamp(0, max_scroll as i32) as usize;
}

fn save_dir_of(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    Some(crate::save::parent_dir(path))
}

fn load_editor_from_cfg() {
    let c = ctx();
    let n = c.editor.sel.min(c.cfg.catalog.len().saturating_sub(1));
    if let Some(e) = c.cfg.catalog.get(n) {
        let (name, path) = (e.name.clone(), e.path.clone());
        c.editor.name.set(name.as_bytes());
        c.editor.path.set(path.as_bytes());
        c.editor.focus = 0;
    } else {
        c.editor.name.set(b"");
        c.editor.path.set(b"");
    }
}

fn persist_cfg() -> bool {
    let c = ctx();
    if c.cfg_path.is_empty() {
        return false;
    }
    crate::config::save_to(&c.cfg_path, &c.cfg).is_ok()
}

fn apply_editor() {
    let c = ctx();
    let name = String::from_utf8_lossy(c.editor.name.as_bytes()).to_string();
    let path = String::from_utf8_lossy(c.editor.path.as_bytes()).to_string();
    if name.trim().is_empty() || path.trim().is_empty() {
        c.editor.note("Имя и путь обязательны: пустое поле не сохраняется");
        return;
    }
    if c.editor.sel < c.cfg.catalog.len() {
        c.cfg.catalog[c.editor.sel].name = name.clone();
        c.cfg.catalog[c.editor.sel].path = path.clone();
    } else {
        c.cfg.catalog.push(crate::config::CatalogEntry { name: name.clone(), path: path.clone() });
    }
    // каталог окна синхронизируется с конфигом: левая панель показывает то же,
    // что лежит в ini, иначе правка «не видна»
    c.ui.catalog_names.clear();
    c.ui.catalog_paths.clear();
    for e in &c.cfg.catalog {
        c.ui.catalog_names.push(e.name.clone());
        c.ui.catalog_paths.push(e.path.clone());
    }
    if persist_cfg() {
        let msg = format!("Сохранено в {}: {} -> {}", c.cfg_path, name, path);
        c.editor.note(&msg);
        set_flash("Каталог сохранён");
    } else {
        c.editor.note("Не удалось записать ini: путь к конфигу не задан");
    }
}

fn open_path(path: &str) {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            set_flash(&format!("Не читается {}: {}", path, e));
            return;
        }
    };
    let mut d = Doc::with_capacity(1024);
    if let Err(e) = crate::dxf::parse(&data, &mut d) {
        set_flash(&format!("Разбор {}: {}", path, e));
        return;
    }
    d.path.push_str(path);
    let c = ctx();
    c.doc = d;
    c.ui.selected_text = if c.doc.texts.is_empty() { None } else { Some(0) };
    c.ui.set_file(path);
    c.renderer.sx.clear();
    c.renderer.sy.clear();
    c.renderer.sx.resize(c.doc.xs.len().max(64) * 2, 0);
    c.renderer.sy.resize(c.doc.xs.len().max(64) * 2, 0);
    let view_w = (c.renderer.fb.w as i32 - crate::render::swiss::LEFT_W - crate::render::swiss::RIGHT_W).max(32);
    let view_h = (c.renderer.fb.h as i32 - crate::render::swiss::TOP_H - crate::render::swiss::STATUS_H).max(32);
    c.zoom = 0.0;
    c.ui.texts_scroll = 0;
    let fit = Camera::fit(c.doc.bounds(), view_w, view_h, 64);
    c.zoom = fit.scale;
    let (x0, y0, x1, y1) = c.doc.bounds();
    c.view_center = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    c.cam = fit;
    let msg = format!("Открыт {}: надписей {}", path, c.doc.texts.len());
    set_flash(&msg);
}

/// Действие «ПРИНЯТЬ» — одно и то же для Enter и для кнопки мышью.
fn submit_modal() {
    let c = ctx();
    match c.session.step {
        crate::edit::Step::Editing { .. } => {
            c.session.commit_text();
            c.focus = 1;
            if c.session.error != WireError::None {
                set_flash(c.session.error.message());
            } else {
                set_flash("Введите номер провода (обязательно), Tab — комментарий");
            }
        }
        crate::edit::Step::AwaitWire { .. } => {
            let o = c.session.submit_wire(&mut c.doc);
            match o {
                Outcome::Applied => {
                    set_flash("Принято. Ctrl+S — сохранить с версией и письмом");
                    c.focus = 0;
                }
                Outcome::Rejected => set_flash(c.session.error.message()),
                _ => {}
            }
        }
        crate::edit::Step::Idle => {}
    }
}

fn do_save() {
    let c = ctx();
    set_flash("Сохранение: версия, .txt, .eml...");
    let msg = match c.save_hook {
        Some(h) => h(&c.doc, &c.cfg),
        None => "сохранение недоступно".to_string(),
    };
    c.ui.set_last(&msg);
    set_flash(&msg);
}

unsafe extern "system" fn wndproc(hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_PAINT => {
            render_and_present();
            0
        }
        WM_GETMINMAXINFO => DefWindowProcA(hwnd, msg, wp, lp),
        WM_MOUSEMOVE => {
            let x = (lp & 0xFFFF) as i16 as i32;
            let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
            let c = ctx();
            let prev = c.ui.mouse;
            c.ui.mouse = (x, y);
            if c.panning {
                let (px, py) = c.pan_from;
                pan_by(x - px, y - py);
                c.pan_from = (x, y);
                InvalidateRect(hwnd, std::ptr::null(), 0);
                return 0;
            }
            // подсветка надписи под курсором — только когда курсор над канвой
            let over_canvas = x > crate::render::swiss::LEFT_W
                && x < (c.renderer.fb.w as i32 - crate::render::swiss::RIGHT_W)
                && y > crate::render::swiss::TOP_H;
            let hover = if over_canvas { hit_test_text(x, y) } else { None };
            if hover != c.ui.hover_text || prev != c.ui.mouse {
                c.ui.hover_text = hover;
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_MBUTTONDOWN | WM_RBUTTONDOWN => {
            let c = ctx();
            let x = (lp & 0xFFFF) as i16 as i32;
            let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
            c.panning = true;
            c.pan_from = (x, y);
            unsafe {
                SetCapture(hwnd);
            }
            set_flash("Панорама: тяните мышью; отпустите кнопку");
            0
        }
        WM_MBUTTONUP | WM_RBUTTONUP => {
            let c = ctx();
            if c.panning {
                c.panning = false;
                unsafe {
                    ReleaseCapture();
                }
                set_flash("Панорама завершена");
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_CAPTURECHANGED => {
            let c = ctx();
            c.panning = false;
            0
        }
        WM_LBUTTONDOWN => {
            let x = (lp & 0xFFFF) as i16 as i32;
            let y = ((lp >> 16) & 0xFFFF) as i16 as i32;
            let c = ctx();
            let mut cr = Rect::default();
            GetClientRect(hwnd, &mut cr);
            let (cw, ch) = (cr.right - cr.left, cr.bottom - cr.top);
            c.ui.mouse = (x, y);

            // ---------- тулбар: самый верхний слой, всегда первый
            for (i, (_, code)) in crate::render::TOOLBAR.iter().enumerate() {
                if crate::render::hit(crate::render::toolbar_rect(i), x, y) {
                    c.ui.pressed_tool = Some(i);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    run_tool(*code);
                    c.ui.pressed_tool = None;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
            }

            // ---------- справка: клик по кнопке формы закрывает её
            if c.ui.help_open {
                let n = 4usize;
                if crate::render::hit(crate::render::form_accept_rect(cw, ch, n), x, y) {
                    c.ui.help_open = false;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                return 0;
            }

            // ---------- клики по форме редактора каталога (приоритет — модалка сверху)
            if c.editor.open {
                let n = 2usize;
                for i in 0..n {
                    let fr = crate::render::form_field_rect(cw, ch, n, i);
                    if crate::render::hit(fr, x, y) {
                        c.editor.focus = i as u8;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                        return 0;
                    }
                }
                let ar = crate::render::form_accept_rect(cw, ch, n);
                if crate::render::hit(ar, x, y) {
                    apply_editor();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                // клик мимо полей формы в открытом редакторе ничего не меняет
                return 0;
            }

            // ---------- клики по модальному шагу правки
            if c.session.is_busy() {
                let L = crate::render::modal_layout(cw, ch);
                if crate::render::hit(L.accept, x, y) {
                    submit_modal();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if crate::render::hit(L.cancel, x, y) {
                    c.session.cancel();
                    set_flash("Правка отменена: файл не тронут");
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if crate::render::hit(L.text, x, y) {
                    c.focus = 0;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if crate::render::hit(L.wire, x, y) {
                    // клик по номеру провода на шаге текста = подтвердить текст и перейти дальше
                    if matches!(c.session.step, crate::edit::Step::Editing { .. }) {
                        c.session.commit_text();
                    }
                    c.focus = 1;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if crate::render::hit(L.comment, x, y) {
                    if matches!(c.session.step, crate::edit::Step::Editing { .. }) {
                        c.session.commit_text();
                    }
                    c.focus = 2;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                // клик мимо полей в модалке не должен «протекать» в выбор надписи
                return 0;
            }

            // 1) левая панель: каталог сверху, список надписей ниже
            if x < crate::render::swiss::LEFT_W && y > crate::render::swiss::TOP_H {
                let cat_rows = c.ui.catalog_names.len().min(6);
                for i in 0..cat_rows {
                    if crate::render::hit(crate::render::left_row_rect(i), x, y) {
                        c.ui.selected_catalog = i;
                        let s = format!("Каталог: {}", c.ui.catalog_names[i]);
                        set_flash(&s);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                        return 0;
                    }
                }
                let cat_bottom = crate::render::left_catalog_bottom(c.ui.catalog_names.len()) + 10;
                if y > cat_bottom + 40 {
                    let row = ((y - cat_bottom - 40) / crate::render::swiss::ROW_H).max(0) as usize;
                    let idx = c.ui.texts_scroll + row;
                    if idx < c.doc.texts.len() {
                        c.ui.selected_text = Some(idx);
                        let s = format!(
                            "Выбрана надпись [{}]: \"{}\" — F2 править",
                            idx,
                            String::from_utf8_lossy(c.doc.arena_get(c.doc.texts[idx].s))
                        );
                        set_flash(&s);
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
            }
            // 2) клик по канве — выбор надписи (с запасом) или сброс
            let hit = hit_test_text(x, y);
            c.ui.selected_text = hit;
            match hit {
                Some(i) => {
                    let s = format!(
                        "Выбрана надпись [{}]: \"{}\" — F2 править, F3 комментарий",
                        i,
                        String::from_utf8_lossy(c.doc.arena_get(c.doc.texts[i].s))
                    );
                    set_flash(&s);
                    // двойной клик по надписи сразу открывает правку
                    if c.last_click_text == Some(i) && c.last_click_ms > 0 {
                        start_edit(false);
                    }
                    c.last_click_text = Some(i);
                    c.last_click_ms = 1;
                }
                None => {
                    set_flash("Надпись не выбрана: кликните по тексту на чертеже");
                    c.last_click_text = None;
                }
            }
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_RBUTTONDOWN => {
            set_flash("F2 — правка · F3 — комментарий · Ctrl+S — сохранить · +/- масштаб");
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_MOUSEWHEEL => {
            // wp приходит в экранных координатах; пользуемся последней позицией
            // курсора из WM_MOUSEMOVE — она в координатах клиента.
            let delta = ((wp >> 16) & 0xFFFF) as i16 as i32;
            let c = ctx();
            let (mx, my) = c.ui.mouse;
            if mx >= 0 && mx < crate::render::swiss::LEFT_W && my > crate::render::swiss::TOP_H {
                scroll_texts(if delta > 0 { -3 } else { 3 });
            } else {
                let f = if delta > 0 { 1.18 } else { 1.0 / 1.18 };
                zoom_at(mx.max(0), my.max(0), f);
            }
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_CHAR => {
            // ВАЖНО: окно зарегистрировано через RegisterClassA, поэтому WM_CHAR
            // приносит БАЙТ кодовой страницы (CP1251), а не Unicode. Чтение его
            // как char давало «?» вместо русских букв — реальный баг.
            let byte = (wp & 0xFF) as u8;
            let c = ctx();
            if byte >= 32 {
                let mut utf8 = Vec::with_capacity(4);
                crate::text::cp1251_to_utf8(&[byte], &mut utf8);
                for ch in String::from_utf8_lossy(&utf8).chars() {
                    if c.editor.open {
                        let b = if c.editor.focus == 1 { &mut c.editor.path } else { &mut c.editor.name };
                        b.insert_char(ch);
                    } else if c.session.is_busy() {
                        current_field_buf().insert_char(ch);
                    }
                }
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_KEYDOWN => {
            let c = ctx();
            let ctrl = unsafe { GetKeyState(VK_CONTROL) } < 0;
            // ---------- редактор каталога перехватывает клавиши первым
            if c.editor.open {
                match wp {
                    VK_ESCAPE => {
                        c.editor.open = false;
                        set_flash("Редактор каталога закрыт");
                    }
                    VK_TAB => {
                        c.editor.focus = (c.editor.focus + 1) % 2;
                    }
                    VK_UP => {
                        if c.editor.sel > 0 {
                            c.editor.sel -= 1;
                            load_editor_from_cfg();
                        }
                    }
                    VK_DOWN => {
                        if c.editor.sel + 1 < c.cfg.catalog.len() {
                            c.editor.sel += 1;
                            load_editor_from_cfg();
                        }
                    }
                    VK_INSERT => {
                        c.cfg.catalog.push(crate::config::CatalogEntry {
                            name: "Новая папка".to_string(),
                            path: String::new(),
                        });
                        c.editor.sel = c.cfg.catalog.len() - 1;
                        load_editor_from_cfg();
                        c.editor.note("Добавлена запись: впишите путь (F5 — выбрать папку)");
                    }
                    VK_DELETE => {
                        if !c.cfg.catalog.is_empty() {
                            let n = c.editor.sel.min(c.cfg.catalog.len() - 1);
                            c.cfg.catalog.remove(n);
                            c.editor.sel = n.saturating_sub(1);
                            load_editor_from_cfg();
                            let _ = persist_cfg();
                            c.editor.note("Запись удалена и каталог сохранён");
                        }
                    }
                    VK_F5 => {
                        let cur = c.editor.path.as_bytes().to_vec();
                        let init = if cur.is_empty() { None } else { Some(String::from_utf8_lossy(&cur).to_string()) };
                        if let Some(p) = dialogs::pick_folder(hwnd, "Папка сетевого диска") {
                            c.editor.path.set(p.as_bytes());
                            c.editor.note("Папка выбрана; Enter — сохранить в ini");
                        } else {
                            c.editor.note("Выбор папки отменён");
                        }
                        let _ = init;
                    }
                    VK_BACK => {
                        let b = if c.editor.focus == 1 { &mut c.editor.path } else { &mut c.editor.name };
                        b.backspace();
                    }
                    VK_RETURN => {
                        apply_editor();
                    }
                    _ => {}
                }
                InvalidateRect(hwnd, std::ptr::null(), 0);
                return 0;
            }
            match wp {
                VK_F2 => {
                    start_edit(false);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                VK_F3 => {
                    start_edit(true);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                VK_TAB => {
                    if c.session.is_busy() {
                        c.focus = (c.focus + 1) % 3;
                        if matches!(c.session.step, Step::Editing { .. }) && c.focus == 1 {
                            c.session.commit_text();
                        }
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                VK_BACK => {
                    if c.session.is_busy() && current_field_buf().backspace() {
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                VK_ESCAPE => {
                    if c.ui.help_open {
                        c.ui.help_open = false;
                        set_flash("Справка закрыта");
                    } else if c.session.is_busy() {
                        let o = c.session.cancel();
                        set_flash(if o == Outcome::Canceled {
                            "Правка отменена: файл не тронут"
                        } else {
                            "Нет активной правки"
                        });
                    } else {
                        unsafe { DestroyWindow(hwnd) };
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0x70 => {
                    // F1 — справка
                    run_tool(7);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0x21 | 0x22 => {
                    // PgUp / PgDn — масштаб вокруг центра канвы
                    let cw = c.renderer.fb.w as i32;
                    let chh = c.renderer.fb.h as i32;
                    let mx = crate::render::swiss::LEFT_W + (cw - crate::render::swiss::LEFT_W - crate::render::swiss::RIGHT_W) / 2;
                    let my = crate::render::swiss::TOP_H + (chh - crate::render::swiss::TOP_H - crate::render::swiss::STATUS_H) / 2;
                    zoom_at(mx, my, if wp == 0x21 { 1.25 } else { 1.0 / 1.25 });
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                VK_RETURN => {
                    if c.session.is_busy() {
                        submit_modal();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                VK_ADD | VK_OEM_PLUS => {
                    let (mx, my) = c.ui.mouse;
                    zoom_at(mx.max(crate::render::swiss::LEFT_W + 40), my.max(crate::render::swiss::TOP_H + 40), 1.25);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                VK_SUBTRACT | VK_OEM_MINUS => {
                    let (mx, my) = c.ui.mouse;
                    zoom_at(mx.max(crate::render::swiss::LEFT_W + 40), my.max(crate::render::swiss::TOP_H + 40), 1.0 / 1.25);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                VK_F4 => {
                    run_tool(2);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                VK_O => {
                    if ctrl {
                        run_tool(1);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                VK_F7 => {
                    fit_view();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0x53 => {
                    // 'S'
                    if ctrl && !c.session.is_busy() {
                        do_save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                _ => {}
            }
            0
        }
        WM_TIMER => unsafe {
            DestroyWindow(hwnd);
            0
        },
        WM_CLOSE => {
            unsafe {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcA(hwnd, msg, wp, lp) },
    }
}

/// Открыть окно. `selftest_ms > 0` — закрыть самому через N мс (проверка запуска
/// без участия человека: используется в тестах и при сборке exe).
pub fn run(
    doc: Doc,
    width: usize,
    height: usize,
    title: &str,
    save_hook: Option<SaveHook>,
    selftest_ms: u32,
    cfg: AppConfig,
    cfg_path: String,
) -> i32 {
    unsafe {
        // Консольное окно в оконном режиме не нужно: exe собран как консольное
        // приложение (чтобы работали команды CLI), поэтому отцепляемся — иначе
        // рядом с окном остаётся чёрное окно консоли.
        let _ = FreeConsole();
        // SetProcessDPIAware НЕ вызываем осознанно: интерфейс рисуется в пикселях
        // 1:1, и на 125–150% DPI системное масштабирование даёт правильный размер
        // (чуть мягче), тогда как принудительный DPI-aware делал всё мелким.
        let _ = SetProcessDPIAware;
        let hinst = GetModuleHandleA(std::ptr::null());
        let class_name = b"dxfsign_window\x00";
        let wc = WndClassA {
            style: 0x0020 | 0x0001, // CS_OWNDC | CS_VREDRAW
            lpfn_wnd_proc: Some(wndproc),
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: hinst,
            h_icon: 0,
            h_cursor: LoadCursorA(0, 32512 as *const u8), // IDC_ARROW
            hbr_background: 0,
            lpsz_menu_name: std::ptr::null(),
            lpsz_class_name: class_name.as_ptr(),
        };
        RegisterClassA(&wc);

        let mut ui = UiState::new();
        ui.set_file(title);
        ui.set_spool(if cfg.spool_dir.is_empty() { "спул не задан" } else { &cfg.spool_dir });
        ui.set_last("ещё не сохраняли");
        for e in &cfg.catalog {
            ui.catalog_names.push(e.name.clone());
            ui.catalog_paths.push(e.path.clone());
        }
        ui.set_status("F2 правка · F3 комментарий · Ctrl+S сохранить · F7 вписать · Esc выход");
        let renderer = Renderer::new(width.max(640), height.max(400), doc.xs.len().max(64) * 2);
        let view_w = (width as i32 - crate::render::swiss::LEFT_W - crate::render::swiss::RIGHT_W).max(32);
        let view_h = (height as i32 - crate::render::swiss::TOP_H - crate::render::swiss::STATUS_H).max(32);
        let cam = Camera::fit(doc.bounds(), view_w, view_h, 32);
        let painter = match GdiPainter::new() {
            Some(p) => p,
            None => return 3,
        };
        let mem_dc = CreateCompatibleDC(0);
        let c = Box::new(Ctx {
            cfg,
            cfg_path,
            editor: CatEditor::new(),
            doc,
            session: EditSession::new(),
            ui,
            renderer,
            painter,
            cam,
            view_center: (0.0, 0.0),
            zoom: 0.0,
            panning: false,
            pan_from: (0, 0),
            hwnd: 0,
            mem_dc,
            bmp: 0,
            old_bmp: 0,
            bits: std::ptr::null_mut(),
            dib_w: 0,
            dib_h: 0,
            focus: 0,
            save_hook,
            flash: [0; 160],
            flash_len: 0,
            last_click_text: None,
            last_click_ms: 0,
        });
        CTX = Box::into_raw(c);

        let mut wtitle = Vec::with_capacity(title.len() + 24);
        wtitle.extend_from_slice(b"DXFSIGN \xe2\x80\x94 ");
        wtitle.extend_from_slice(title.as_bytes());
        wtitle.push(0);

        let hwnd = CreateWindowExA(
            0,
            class_name.as_ptr(),
            wtitle.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width as i32,
            height as i32,
            0,
            0,
            hinst,
            std::ptr::null_mut(),
        );
        if hwnd == 0 {
            return 4;
        }
        ctx().hwnd = hwnd;
        // первичный выбор первой надписи: человек сразу видит, что можно править
        if !ctx().doc.texts.is_empty() {
            ctx().ui.selected_text = Some(0);
        }
        ShowWindow(hwnd, SW_SHOWMAXIMIZED);
        UpdateWindow(hwnd);
        if selftest_ms > 0 {
            SetTimer(hwnd, 1, selftest_ms, std::ptr::null());
        }
        let mut msg = Msg::default();
        while GetMessageA(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }
        let _ = SetWindowTextA(hwnd, std::ptr::null());
        0
    }
}
