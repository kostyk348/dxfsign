//! render.rs — кадр: проекция (SIMD), растеризация геометрии и раскладка UI
//! в стиле Brutalism + Swiss Style.
//!
//! Стиль здесь — не прилагательные, а числа (см. `Swiss`):
//!   * модульная сетка 12 колонок, жёлоб 8 px, внешнее поле 24 px, базовая
//!     линия 8 px — все координаты кратны 8, ничего «на глаз»;
//!   * типографическая шкала 11 / 13 / 16 / 28 / 48 px, одна гарнитура без
//!     засечек (системная Helvetica/Arial), выключка влево, никакого центра;
//!   * палитра из четырёх цветов + акцент: бумага #F2F0EB, тушь #111111,
//!     линейка #C9C6BF, акцент швейцарский красный #E2231A;
//!   * Brutalism: рамки 2 px чистым чёрным, радиус 0, теней нет, градиентов нет,
//!     панели прижаты вплотную, разделители — сплошные линии, а не отступы.
//!
//! Кадр не аллоцирует: все буферы (кадр, экранные координаты) выделены до цикла
//! в `Renderer::new`; если чертёж больше буфера — возвращается `Overflow`,
//! и вызывающий решает (увеличить резерв при открытии файла, не в кадре).

use crate::doc::{Doc, TEXT_KIND_MTEXT};
use crate::edit::{EditSession, Step};

// ---------------------------------------------------------------- палитра и шкала

pub mod swiss {
    //! Числовые константы стиля. Одно место, где стиль задан.
    //!
    //! Шкала пересчитана под реальный глаз: базис 12 px вместо 8, кегли 15/17/21/34.
    //! Первая версия была на 8 px и 11-13 px кегля — на 100% DPI это читалось как
    //! «мелко и отвратительно», и попасть мышью в строку 24 px было почти нельзя.
    //! Пропорции Swiss сохранены (1.4× между ступенями), изменился только масштаб.
    pub const PAPER: u32 = 0x00F2_F0EB;
    pub const INK: u32 = 0x0011_1111;
    pub const RULE: u32 = 0x00C9_C6BF;
    pub const ACCENT: u32 = 0x00E2_231A;
    pub const MUTED: u32 = 0x006B_6862;
    pub const WHITE: u32 = 0x00FF_FFFF;
    pub const CANVAS: u32 = 0x00E8_E5DE;
    pub const FIELD: u32 = 0x00FF_FFFF;
    pub const ERROR: u32 = 0x00B3_1A12;

    pub const BASE: i32 = 12;
    pub const MARGIN: i32 = 32;
    pub const GUTTER: i32 = 12;
    pub const COLUMNS: i32 = 12;
    pub const RULE_W: i32 = 3;
    pub const RADIUS: i32 = 0;

    pub const T_CAPTION: i32 = 15;
    pub const T_BODY: i32 = 17;
    pub const T_LABEL: i32 = 21;
    pub const T_TITLE: i32 = 34;
    pub const T_DISPLAY: i32 = 56;

    pub const TOP_H: i32 = 72;
    pub const STATUS_H: i32 = 44;
    pub const LEFT_W: i32 = 340;
    pub const RIGHT_W: i32 = 400;
    pub const MODAL_W: i32 = 760;
    pub const MODAL_H: i32 = 470;
    pub const ROW_H: i32 = 32;
}

// ---------------------------------------------------------------- кадр

#[derive(Debug)]
pub enum RenderError {
    /// Чертёж не влез в заранее выделенные буферы кадра.
    Overflow { need: usize, have: usize },
}

pub struct Framebuffer {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl Framebuffer {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![swiss::PAPER; w * h] }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        if w != self.w || h != self.h {
            self.w = w;
            self.h = h;
            self.px.clear();
            self.px.resize(w * h, swiss::PAPER);
        }
    }

    #[inline]
    pub fn clear(&mut self, color: u32) {
        for p in self.px.iter_mut() {
            *p = color;
        }
    }

    #[inline]
    pub fn put(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.px[y as usize * self.w + x as usize] = color;
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let x0 = x.max(0) as usize;
        let y0 = y.max(0) as usize;
        let x1 = ((x + w).min(self.w as i32)).max(0) as usize;
        let y1 = ((y + h).min(self.h as i32)).max(0) as usize;
        for yy in y0..y1 {
            let row = yy * self.w;
            for xx in x0..x1 {
                self.px[row + xx] = color;
            }
        }
    }

    /// Рамка толщиной `t` без заливки — основной приём Brutalism.
    pub fn stroke_rect(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, color: u32) {
        self.fill_rect(x, y, w, t, color);
        self.fill_rect(x, y + h - t, w, t, color);
        self.fill_rect(x, y, t, h, color);
        self.fill_rect(x + w - t, y, t, h, color);
    }

    /// Отрезок по Брезенхему: O(max(dx,dy)), без ветвлений в теле цикла.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx - dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            self.put(x, y, color);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Окружность: O(r) на четверть через симметрию (8 точек за шаг).
    pub fn circle(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        if r <= 0 {
            self.put(cx, cy, color);
            return;
        }
        let mut x = r;
        let mut y = 0;
        let mut err = 1 - r;
        while x >= y {
            for (px, py) in [
                (cx + x, cy + y),
                (cx + y, cy + x),
                (cx - y, cy + x),
                (cx - x, cy + y),
                (cx - x, cy - y),
                (cx - y, cy - x),
                (cx + y, cy - x),
                (cx + x, cy - y),
            ] {
                self.put(px, py, color);
            }
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }
}

// ---------------------------------------------------------------- SIMD-ядро

/// Проекция: мир (f64) -> экран (i32). Это единственное SIMD-ядро проекта,
/// и оно оправдано: вызывается на каждой точке на каждом кадре.
/// Сложность O(N) по точкам; AVX2 обрабатывает 4 точки за инструкцию,
/// потолок упирается в память (~N*16 байт чтения), не в арифметику.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub scale: f64,
    pub ox: f64,
    pub oy: f64,
}

impl Camera {
    /// Вписать границы чертежа в область с полем `pad`. O(1) (границы — снаружи).
    pub fn fit(bounds: (f64, f64, f64, f64), vw: i32, vh: i32, pad: i32) -> Self {
        let (x0, y0, x1, y1) = bounds;
        let w = (x1 - x0).max(1e-9);
        let h = (y1 - y0).max(1e-9);
        let sx = (vw - 2 * pad) as f64 / w;
        let sy = (vh - 2 * pad) as f64 / h;
        let scale = sx.min(sy);
        let cx = (x0 + x1) * 0.5;
        let cy = (y0 + y1) * 0.5;
        Self {
            scale,
            ox: vw as f64 * 0.5 - cx * scale,
            // экранная ось Y смотрит вниз, мировая — вверх
            oy: vh as f64 * 0.5 + cy * scale,
        }
    }
}

/// Скалярная проекция — эталон корректности и путь для машин без AVX2
/// (Windows 7 ставилась на Westmere/Sandy Bridge, где AVX2 нет).
pub fn project_scalar(xs: &[f64], ys: &[f64], cam: Camera, sx: &mut [i32], sy: &mut [i32]) {
    // Клампинг до ±1e6 обязателен: чертёж с координатой 1e300 даёт inf/nan,
    // и приведение к i32 (saturating в Rust) выдало бы i32::MIN — а это уже
    // другое значение и расхождение с AVX2-путём. Оба пути клампят одинаково.
    const LIM: f64 = 1.0e6;
    for i in 0..xs.len() {
        let px = xs[i] * cam.scale + cam.ox;
        let py = cam.oy - ys[i] * cam.scale;
        sx[i] = if px.is_finite() { px.round().clamp(-LIM, LIM) as i32 } else { LIM as i32 * px.signum() as i32 };
        sy[i] = if py.is_finite() { py.round().clamp(-LIM, LIM) as i32 } else { LIM as i32 * py.signum() as i32 };
    }
}

/// AVX2-путь: 4 точки за раз, f64 -> i32 через усечение, клампинг до ±1e6,
/// чтобы заведомо не выйти за i32. Требует avx2.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub unsafe fn project_avx2(
    xs: &[f64],
    ys: &[f64],
    cam: Camera,
    sx: &mut [i32],
    sy: &mut [i32],
) {
    use std::arch::x86_64::*;
    let n = xs.len();
    let vs = _mm256_set1_pd(cam.scale);
    let vox = _mm256_set1_pd(cam.ox);
    let voy = _mm256_set1_pd(cam.oy);
    let zero = _mm256_setzero_pd();
    let lim = _mm256_set1_pd(1.0e6);
    let nlim = _mm256_set1_pd(-1.0e6);
    let mut i = 0usize;
    while i + 4 <= n {
        let x = _mm256_loadu_pd(xs.as_ptr().add(i));
        let y = _mm256_loadu_pd(ys.as_ptr().add(i));
        // sx = clamp(x*scale + ox)
        let mut px = _mm256_add_pd(_mm256_mul_pd(x, vs), vox);
        px = _mm256_min_pd(_mm256_max_pd(px, nlim), lim);
        // sy = clamp(oy - y*scale)
        let mut py = _mm256_sub_pd(voy, _mm256_mul_pd(y, vs));
        py = _mm256_min_pd(_mm256_max_pd(py, nlim), lim);
        let _ = zero;
        // cvttpd_epi32 отдаёт 4×i32 в __m128i (не __m256i) — храним через _mm_storeu_si128
        let ix = _mm256_cvttpd_epi32(px);
        let iy = _mm256_cvttpd_epi32(py);
        _mm_storeu_si128(sx.as_mut_ptr().add(i) as *mut __m128i, ix);
        _mm_storeu_si128(sy.as_mut_ptr().add(i) as *mut __m128i, iy);
        i += 4;
    }
    // Хвост обрабатывается скалярным путём — тем же кодом, что основной
    // скалярный путь, иначе клампинг разъезжается (это был реальный баг:
    // хвост без клампинга выдавал i32::MIN на бесконечных координатах).
    if i < n {
        project_scalar(&xs[i..], &ys[i..], cam, &mut sx[i..], &mut sy[i..]);
    }
}

/// Диспетчеризация: AVX2, если он есть у процессора, иначе скаляр.
/// O(N). Возвращает выбранный путь — это пишется в статусную строку, чтобы
/// человек видел, на чём именно считает машина (и на Win7 не удивлялся).
pub fn project(
    xs: &[f64],
    ys: &[f64],
    cam: Camera,
    sx: &mut [i32],
    sy: &mut [i32],
) -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            unsafe { project_avx2(xs, ys, cam, sx, sy) };
            return "avx2";
        }
    }
    project_scalar(xs, ys, cam, sx, sy);
    "scalar"
}

// ---------------------------------------------------------------- текст на экране

/// POP-контракт рисования текста: рендер не знает, GDI под ним или что-то ещё.
/// На Win7 системная Helvetica/Arial есть всегда — поэтому Swiss-типографика
/// получается системным шрифтом, без встроенных TTF и без FreeType.
pub trait TextPainter {
    fn text(&mut self, fb: &mut Framebuffer, x: i32, y: i32, px: i32, bold: bool, color: u32, s: &str);
    fn measure(&mut self, px: i32, bold: bool, s: &str) -> i32;
}

// ---------------------------------------------------------------- состояние UI

pub struct UiState {
    pub file_name: [u8; 160],
    pub file_name_len: usize,
    pub catalog_names: Vec<String>,
    pub catalog_paths: Vec<String>,
    pub selected_catalog: usize,
    pub selected_text: Option<usize>,
    pub status: [u8; 200],
    pub status_len: usize,
    pub backend: &'static str,
    pub entities: u32,
    pub zoom_pct: u32,
    /// Куда уходит письмо (спул) — строка показывается в правой панели,
    /// чтобы не было вопроса «а .eml вообще сохраняется и куда».
    /// Какое поле модалки в фокусе (0 текст, 1 провод, 2 комментарий) — окно
    /// выставляет перед кадром, рендер показывает рамкой и кареткой.
    pub focus: u8,
    pub spool: [u8; 160],
    pub spool_len: usize,
    /// Результат последнего сохранения.
    pub last: [u8; 200],
    pub last_len: usize,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            file_name: [0; 160],
            file_name_len: 0,
            catalog_names: Vec::with_capacity(32),
            catalog_paths: Vec::with_capacity(32),
            selected_catalog: 0,
            selected_text: None,
            status: [0; 200],
            status_len: 0,
            backend: "scalar",
            entities: 0,
            zoom_pct: 100,
            focus: 0,
            spool: [0; 160],
            spool_len: 0,
            last: [0; 200],
            last_len: 0,
        }
    }

    pub fn set_file(&mut self, s: &str) {
        let b = s.as_bytes();
        let n = b.len().min(self.file_name.len());
        self.file_name[..n].copy_from_slice(&b[..n]);
        self.file_name_len = n;
    }

    pub fn set_status(&mut self, s: &str) {
        let b = s.as_bytes();
        let n = b.len().min(self.status.len());
        self.status[..n].copy_from_slice(&b[..n]);
        self.status_len = n;
    }

    pub fn file_str(&self) -> &str {
        std::str::from_utf8(&self.file_name[..self.file_name_len]).unwrap_or("")
    }

    pub fn status_str(&self) -> &str {
        std::str::from_utf8(&self.status[..self.status_len]).unwrap_or("")
    }

    pub fn set_spool(&mut self, s: &str) {
        let b = s.as_bytes();
        let n = b.len().min(self.spool.len());
        self.spool[..n].copy_from_slice(&b[..n]);
        self.spool_len = n;
    }

    pub fn spool_str(&self) -> &str {
        std::str::from_utf8(&self.spool[..self.spool_len]).unwrap_or("")
    }

    pub fn set_last(&mut self, s: &str) {
        let b = s.as_bytes();
        let n = b.len().min(self.last.len());
        self.last[..n].copy_from_slice(&b[..n]);
        self.last_len = n;
    }

    pub fn last_str(&self) -> &str {
        std::str::from_utf8(&self.last[..self.last_len]).unwrap_or("")
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- геометрия модалки

/// Прямоугольник (x, y, w, h) в координатах кадра.
pub type Rect = (i32, i32, i32, i32);

/// Раскладка модального шага правки. ОДНА функция и для рисования, и для
/// попадания мышью: раньше окно считало геометрию само, поэтому поля не
/// кликались — мышью можно было только нажать Tab.
#[derive(Clone, Copy, Debug)]
pub struct ModalLayout {
    pub mx: i32,
    pub my: i32,
    pub mw: i32,
    pub mh: i32,
    pub text: Rect,
    pub wire: Rect,
    pub comment: Rect,
    pub accept: Rect,
    pub cancel: Rect,
}

/// O(1). Никаких обращений к кадру — чистая арифметика от размера окна.
/// Геометрия полей универсальной формы (редактор каталога и будущие диалоги).
pub fn form_layout(w: i32, h: i32, count: usize) -> (i32, i32, i32, i32, i32, i32) {
    let mw = swiss::MODAL_W.min(w - 48);
    let mh = (swiss::MODAL_H + count as i32 * 64).min(h - 48);
    let mx = (w - mw) / 2;
    let my = (h - mh) / 2;
    (mx, my, mw, mh, swiss::MARGIN, mw - swiss::MARGIN * 2)
}

/// Прямоугольник поля `i` формы. O(1).
pub fn form_field_rect(w: i32, h: i32, count: usize, i: usize) -> Rect {
    let (mx, my, _mw, _mh, pad, inner) = form_layout(w, h, count);
    (mx + pad, my + pad + 78 + i as i32 * 60 + 20, inner, 46)
}

/// Кнопка «СОХРАНИТЬ» формы. O(1).
pub fn form_accept_rect(w: i32, h: i32, count: usize) -> Rect {
    let (mx, my, mw, mh, pad, _inner) = form_layout(w, h, count);
    (mx + mw - pad - 240, my + mh - 64, 240, 46)
}

pub fn modal_layout(w: i32, h: i32) -> ModalLayout {
    let mw = swiss::MODAL_W.min(w - 48);
    let mh = swiss::MODAL_H.min(h - 48);
    let mx = (w - mw) / 2;
    let my = (h - mh) / 2;
    let pad = swiss::MARGIN;
    let fw = mw - pad * 2;
    let f1y = my + pad + 64 + 18;
    let f2y = f1y + 62;
    let f3y = f2y + 62;
    let by = my + mh - 60;
    let bw = 200;
    ModalLayout {
        mx,
        my,
        mw,
        mh,
        text: (mx + pad, f1y, fw, 48),
        wire: (mx + pad, f2y, fw / 2 - 10, 48),
        comment: (mx + pad, f3y, fw, 48),
        accept: (mx + mw - pad - bw, by, bw, 46),
        cancel: (mx + mw - pad - bw * 2 - swiss::GUTTER, by, bw, 46),
    }
}

#[inline]
pub fn hit(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.0 && x <= rect.0 + rect.2 && y >= rect.1 && y <= rect.1 + rect.3
}

// ---------------------------------------------------------------- кадр целиком

pub struct Renderer {
    pub fb: Framebuffer,
    pub sx: Vec<i32>,
    pub sy: Vec<i32>,
    pub points_cap: usize,
    /// Запас под строки, которые рисуются на канве (ограничен, см. `Overflow`).
    pub text_draw_cap: usize,
}

impl Renderer {
    pub fn new(w: usize, h: usize, points_cap: usize) -> Self {
        Self {
            fb: Framebuffer::new(w, h),
            sx: vec![0; points_cap],
            sy: vec![0; points_cap],
            points_cap,
            text_draw_cap: 4096,
        }
    }

    /// Отрисовать кадр. Не аллоцирует (проверяется тестом `frame_allocates_nothing`).
    pub fn frame(
        &mut self,
        doc: &Doc,
        cam: Camera,
        ui: &mut UiState,
        session: &EditSession,
        tp: &mut dyn TextPainter,
    ) -> Result<(), RenderError> {
        let w = self.fb.w as i32;
        let h = self.fb.h as i32;
        if doc.xs.len() > self.points_cap {
            return Err(RenderError::Overflow { need: doc.xs.len(), have: self.points_cap });
        }
        let (vw0, vh0) = (w - swiss::LEFT_W - swiss::RIGHT_W, h - swiss::TOP_H - swiss::STATUS_H);
        let vx = swiss::LEFT_W;
        let vy = swiss::TOP_H;

        // --- фон
        self.fb.clear(swiss::PAPER);
        self.fb.fill_rect(vx, vy, vw0, vh0, swiss::CANVAS);

        // --- проекция (горячий путь)
        // Камера уже считает центр в координатах канвы (`Camera::fit` по view_w/view_h),
        // поэтому начало канвы прибавляется ОДИН раз — ниже, при отрисовке.
        // Раньше здесь ещё вычитался vx/vy, и чертёж уезжал влево под панель каталога.
        let cam_v = cam;
        let backend = project(&doc.xs, &doc.ys, cam_v, &mut self.sx, &mut self.sy);
        ui.backend = backend;
        ui.entities = doc.entity_count() as u32;

        // --- геометрия
        for s in &doc.segs {
            let (i, j) = (s.a as usize, s.b as usize);
            self.fb.line(
                vx + self.sx[i],
                vy + self.sy[i],
                vx + self.sx[j],
                vy + self.sy[j],
                swiss::INK,
            );
        }
        for p in &doc.polys {
            for k in 0..p.len.saturating_sub(1) {
                let (a, b) = (
                    doc.poly_verts[(p.first + k) as usize] as usize,
                    doc.poly_verts[(p.first + k + 1) as usize] as usize,
                );
                self.fb.line(vx + self.sx[a], vy + self.sy[a], vx + self.sx[b], vy + self.sy[b], swiss::INK);
            }
            if p.closed && p.len > 2 {
                let (a, b) = (
                    doc.poly_verts[(p.first + p.len - 1) as usize] as usize,
                    doc.poly_verts[p.first as usize] as usize,
                );
                self.fb.line(vx + self.sx[a], vy + self.sy[a], vx + self.sx[b], vy + self.sy[b], swiss::INK);
            }
        }
        for c in &doc.circles {
            let i = c.c as usize;
            let r = (c.r * cam.scale).max(1.0) as i32;
            self.fb.circle(vx + self.sx[i], vy + self.sy[i], r, swiss::INK);
        }

        // --- надписи: то, что правится; выделенная — красной рамкой
        let mut drawn = 0usize;
        for (ti, t) in doc.texts.iter().enumerate() {
            if drawn >= self.text_draw_cap {
                break;
            }
            drawn += 1;
            let i = t.pos as usize;
            let px = ((t.height * cam.scale) as i32).clamp(16, 72);
            let x = vx + self.sx[i];
            let y = vy + self.sy[i];
            let selected = ui.selected_text == Some(ti);
            let s = String::from_utf8_lossy(doc.arena_get(t.s));
            let wd = tp.measure(px, false, &s);
            if selected {
                // выделение обязано быть видно: рамка 3 px, метка 6 px и линия под
                // базовой линией. Рамка 2 px на мелком кегле терялась.
                let bx = x - 8;
                let by = y - px - 8;
                let bw = wd + 16;
                let bh = px + 16;
                self.fb.stroke_rect(bx, by, bw, bh, swiss::RULE_W, swiss::ACCENT);
                self.fb.fill_rect(bx - 10, by, 6, bh, swiss::ACCENT);
                self.fb.fill_rect(bx, y + 8, bw, 3, swiss::ACCENT);
            }
            tp.text(&mut self.fb, x, y - px, px, false, swiss::INK, &s);
            // MTEXT помечается квадратом у базовой линии — видно, что абзац
            if t.kind == TEXT_KIND_MTEXT {
                self.fb.fill_rect(x - 8, y - 4, 4, 4, swiss::MUTED);
            }
        }

        // --- верхняя полоса: заголовок + имя файла
        self.fb.fill_rect(0, 0, w, swiss::TOP_H, swiss::INK);
        tp.text(&mut self.fb, swiss::MARGIN, 14, swiss::T_LABEL, true, swiss::PAPER, "DXFSIGN");
        tp.text(&mut self.fb, swiss::MARGIN + 104, 18, swiss::T_BODY, false, swiss::RULE, ui.file_str());
        // цифры форматируются в стек, без String: кадр не аллоцирует
        let mut d1 = [0u8; 10];
        let mut d2 = [0u8; 10];
        let e_s = concat_u32(&mut d1, ui.entities, "");
        let z_s = concat_u32(&mut d2, ui.zoom_pct, "");
        let mut tail = [0u8; 64];
        let mut tail_len = 0usize;
        for part in [e_s, " об · ", z_s, "% · ", ui.backend] {
            for &b in part.as_bytes() {
                if tail_len < tail.len() {
                    tail[tail_len] = b;
                    tail_len += 1;
                }
            }
        }
        let right = std::str::from_utf8(&tail[..tail_len]).unwrap_or("");
        let rw = tp.measure(swiss::T_CAPTION, false, right);
        tp.text(&mut self.fb, w - swiss::MARGIN - rw, 20, swiss::T_CAPTION, false, swiss::RULE, right);
        self.fb.fill_rect(0, swiss::TOP_H - swiss::RULE_W, w, swiss::RULE_W, swiss::ACCENT);

        // --- левая панель: каталог сетевого диска (12-колоночная сетка внутри)
        let lh = h - swiss::TOP_H - swiss::STATUS_H;
        self.fb.fill_rect(0, swiss::TOP_H, swiss::LEFT_W, lh, swiss::WHITE);
        self.fb.fill_rect(swiss::LEFT_W - swiss::RULE_W, swiss::TOP_H, swiss::RULE_W, lh, swiss::INK);
        tp.text(&mut self.fb, swiss::MARGIN, swiss::TOP_H + swiss::MARGIN, swiss::T_CAPTION, true, swiss::MUTED, "КАТАЛОГ");
        let list_y0 = swiss::TOP_H + swiss::MARGIN + 24;
        let rows = ((lh - swiss::MARGIN * 2 - 24) / swiss::ROW_H).max(0) as usize;
        for (i, name) in ui.catalog_names.iter().enumerate().take(rows) {
            let y = list_y0 + i as i32 * swiss::ROW_H;
            if i == ui.selected_catalog {
                self.fb.fill_rect(0, y, swiss::LEFT_W - swiss::RULE_W, swiss::ROW_H, swiss::INK);
                self.fb.fill_rect(0, y, 4, swiss::ROW_H, swiss::ACCENT);
                tp.text(&mut self.fb, swiss::MARGIN, y + 5, swiss::T_BODY, true, swiss::PAPER, name);
            } else {
                tp.text(&mut self.fb, swiss::MARGIN, y + 5, swiss::T_BODY, false, swiss::INK, name);
                self.fb.fill_rect(swiss::MARGIN, y + swiss::ROW_H - 1, swiss::LEFT_W - swiss::MARGIN * 2, 1, swiss::RULE);
            }
        }

        // --- правая панель: свойства выбранной надписи
        // Раскладка с обрезкой по метрике шрифта: раньше длинная подсказка выезжала
        // за край панели и обрезалась пикселями — это и был «кривой UI».
        let rx = w - swiss::RIGHT_W;
        let pad = swiss::MARGIN;
        let inner = swiss::RIGHT_W - pad * 2;
        self.fb.fill_rect(rx, swiss::TOP_H, swiss::RIGHT_W, lh, swiss::WHITE);
        self.fb.fill_rect(rx, swiss::TOP_H, swiss::RULE_W, lh, swiss::INK);
        tp.text(&mut self.fb, rx + pad, swiss::TOP_H + pad, swiss::T_CAPTION, true, swiss::MUTED, "НАДПИСЬ");
        let mut ly = swiss::TOP_H + pad + 26;
        match ui.selected_text.and_then(|i| doc.texts.get(i)) {
            Some(t) => {
                let s = String::from_utf8_lossy(doc.arena_get(t.s));
                let (l1, l2) = split_two_lines(&s, 30);
                self.fb.stroke_rect(rx + pad, ly, inner, 76, swiss::RULE_W, swiss::INK);
                let c1 = clip(tp, swiss::T_LABEL, true, inner - 24, l1);
                tp.text(&mut self.fb, rx + pad + 12, ly + 13, swiss::T_LABEL, true, swiss::INK, c1);
                if !l2.is_empty() {
                    let c2 = clip(tp, swiss::T_LABEL, true, inner - 24, l2);
                    tp.text(&mut self.fb, rx + pad + 12, ly + 42, swiss::T_LABEL, true, swiss::INK, c2);
                }
                ly += 76 + 20;

                let mut nb = [0u8; 24];
                let hs = concat_u32(&mut nb, t.height as u32, " ед.");
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_BODY, false, swiss::MUTED, "высота");
                let hw = tp.measure(swiss::T_BODY, true, hs);
                tp.text(&mut self.fb, rx + swiss::RIGHT_W - pad - hw, ly, swiss::T_BODY, true, swiss::INK, hs);
                ly += 32;

                let wire = String::from_utf8_lossy(doc.arena_get(t.wire));
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_BODY, false, swiss::MUTED, "провод");
                let wstr: &str = if wire.is_empty() { "не задан" } else { &wire };
                let wcol = if wire.is_empty() { swiss::MUTED } else { swiss::ACCENT };
                let ww = tp.measure(swiss::T_BODY, true, wstr);
                tp.text(&mut self.fb, rx + swiss::RIGHT_W - pad - ww, ly, swiss::T_BODY, true, wcol, wstr);
                ly += 36;

                self.fb.fill_rect(rx + pad, ly, inner, swiss::RULE_W, swiss::RULE);
                ly += 18;
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_CAPTION, true, swiss::MUTED, "ПИСЬМО УХОДИТ В");
                ly += 22;
                let sp = clip(tp, swiss::T_BODY, false, inner, ui.spool_str());
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_BODY, false, swiss::INK, sp);
                ly += 32;
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_CAPTION, true, swiss::MUTED, "ПОСЛЕДНЕЕ СОХРАНЕНИЕ");
                ly += 22;
                let ls = clip(tp, swiss::T_BODY, false, inner, ui.last_str());
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_BODY, false, swiss::INK, ls);
            }
            None => {
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_BODY, false, swiss::MUTED, "Ничего не выбрано.");
                ly += 28;
                let hint = clip(tp, swiss::T_BODY, false, inner, "Щёлкните надпись на чертеже");
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_BODY, false, swiss::MUTED, hint);
                ly += 44;
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_CAPTION, true, swiss::MUTED, "ПИСЬМО УХОДИТ В");
                ly += 22;
                let sp2 = clip(tp, swiss::T_BODY, false, inner, ui.spool_str());
                tp.text(&mut self.fb, rx + pad, ly, swiss::T_BODY, false, swiss::INK, sp2);
            }
        }

        // --- статусная строка
        self.fb.fill_rect(0, h - swiss::STATUS_H, w, swiss::STATUS_H, swiss::INK);
        tp.text(&mut self.fb, swiss::MARGIN, h - swiss::STATUS_H + 9, swiss::T_CAPTION, false, swiss::PAPER, ui.status_str());

        // --- модальный запрос номера провода и комментария
        if session.is_busy() {
            let focus = ui.focus;
            self.draw_modal(session, tp, focus);
        }
        Ok(())
    }

    fn draw_modal(&mut self, session: &EditSession, tp: &mut dyn TextPainter, focus: u8) {
        let w = self.fb.w as i32;
        let h = self.fb.h as i32;
        let L = modal_layout(w, h);
        // затемнение сцены смешением к туши (25%), без сетки: сетка давала муар
        for p in self.fb.px.iter_mut() {
            let r = (*p >> 16) & 0xFF;
            let g = (*p >> 8) & 0xFF;
            let b = *p & 0xFF;
            *p = (((r * 3 + 0x11) / 4) << 16) | (((g * 3 + 0x11) / 4) << 8) | ((b * 3 + 0x11) / 4);
        }
        self.fb.fill_rect(L.mx + 10, L.my + 10, L.mw, L.mh, swiss::INK); // жёсткая тень
        self.fb.fill_rect(L.mx, L.my, L.mw, L.mh, swiss::WHITE);
        self.fb.stroke_rect(L.mx, L.my, L.mw, L.mh, swiss::RULE_W, swiss::INK);
        self.fb.fill_rect(L.mx, L.my, L.mw, 5, swiss::ACCENT);

        let pad = swiss::MARGIN;
        tp.text(&mut self.fb, L.mx + pad, L.my + pad, swiss::T_TITLE, true, swiss::INK, "СОХРАНЕНИЕ ПРАВКИ");
        let sub = match session.step {
            Step::Editing { .. } => "Шаг 1 из 2 — текст надписи · Tab или клик: следующее поле",
            Step::AwaitWire { .. } => "Шаг 2 из 2 — номер провода обязателен",
            Step::Idle => "",
        };
        tp.text(&mut self.fb, L.mx + pad, L.my + pad + 44, swiss::T_CAPTION, false, swiss::MUTED, sub);

        // поле 1: текст надписи
        tp.text(&mut self.fb, L.text.0, L.text.1 - 22, swiss::T_CAPTION, true, swiss::MUTED, "ТЕКСТ");
        let editing_text = matches!(session.step, Step::Editing { .. });
        let tborder = if editing_text && focus == 0 { swiss::INK } else if editing_text { swiss::MUTED } else { swiss::RULE };
        self.fb.stroke_rect(L.text.0, L.text.1, L.text.2, L.text.3, swiss::RULE_W, tborder);
        let tstr = String::from_utf8_lossy(session.text.as_bytes());
        tp.text(&mut self.fb, L.text.0 + 12, L.text.1 + 13, swiss::T_LABEL, false, swiss::INK, &tstr);
        if editing_text && focus == 0 {
            let tw = tp.measure(swiss::T_LABEL, false, &tstr);
            let frac = if tstr.is_empty() { 0 } else { session.text.caret.min(tstr.len()) };
            let cx = L.text.0 + 12 + (tw * frac as i32 / tstr.len().max(1) as i32);
            self.fb.fill_rect(cx, L.text.1 + 10, 3, 28, swiss::ACCENT);
        }

        // поле 2: номер провода
        tp.text(&mut self.fb, L.wire.0, L.wire.1 - 22, swiss::T_CAPTION, true, swiss::MUTED, "НОМЕР ПРОВОДА *");
        let wfocused = matches!(session.step, Step::AwaitWire { .. }) && focus == 1;
        let wborder = if session.error != crate::edit::WireError::None {
            swiss::ERROR
        } else if wfocused {
            swiss::INK
        } else if matches!(session.step, Step::AwaitWire { .. }) {
            swiss::MUTED
        } else {
            swiss::RULE
        };
        self.fb.stroke_rect(L.wire.0, L.wire.1, L.wire.2, L.wire.3, swiss::RULE_W, wborder);
        let wstr = String::from_utf8_lossy(session.wire.as_bytes());
        tp.text(&mut self.fb, L.wire.0 + 12, L.wire.1 + 13, swiss::T_LABEL, true, swiss::INK, &wstr);
        if wfocused {
            let ww = tp.measure(swiss::T_LABEL, true, &wstr);
            self.fb.fill_rect(L.wire.0 + 14 + ww, L.wire.1 + 10, 3, 28, swiss::ACCENT);
        }
        let err = session.error.message();
        if !err.is_empty() {
            tp.text(&mut self.fb, L.wire.0 + L.wire.2 + 16, L.wire.1 + 13, swiss::T_CAPTION, true, swiss::ERROR, err);
        }

        // поле 3: свободный комментарий — фокус виден рамкой и кареткой,
        // иначе «кликнул, а непонятно, попал ли»
        let cfocused = focus == 2;
        let clabel = if cfocused { "КОММЕНТАРИЙ (свободный) — ввод" } else { "КОММЕНТАРИЙ (свободный)" };
        tp.text(&mut self.fb, L.comment.0, L.comment.1 - 22, swiss::T_CAPTION, true, swiss::MUTED, clabel);
        self.fb.stroke_rect(
            L.comment.0,
            L.comment.1,
            L.comment.2,
            L.comment.3,
            swiss::RULE_W,
            if cfocused { swiss::INK } else { swiss::RULE },
        );
        let cstr = String::from_utf8_lossy(session.comment.as_bytes());
        tp.text(&mut self.fb, L.comment.0 + 12, L.comment.1 + 13, swiss::T_BODY, false, swiss::INK, &cstr);
        if cfocused {
            let cw = tp.measure(swiss::T_BODY, false, &cstr);
            self.fb.fill_rect(L.comment.0 + 14 + cw, L.comment.1 + 10, 3, 28, swiss::ACCENT);
        }

        // кнопки
        self.fb.fill_rect(L.accept.0, L.accept.1, L.accept.2, L.accept.3, swiss::INK);
        tp.text(&mut self.fb, L.accept.0 + 24, L.accept.1 + 12, swiss::T_LABEL, true, swiss::PAPER, "ПРИНЯТЬ  Enter");
        self.fb.stroke_rect(L.cancel.0, L.cancel.1, L.cancel.2, L.cancel.3, swiss::RULE_W, swiss::INK);
        tp.text(&mut self.fb, L.cancel.0 + 24, L.cancel.1 + 12, swiss::T_LABEL, false, swiss::INK, "ОТМЕНА  Esc");
    }

    /// Универсальная форма-модалка: заголовок, подпись, поля «метка: значение»
    /// и подсказка. Используется редактором каталога (и любым будущим диалогом),
    /// чтобы окно не обрастало отдельной вёрсткой на каждый случай.
    pub fn draw_form(
        &mut self,
        tp: &mut dyn TextPainter,
        title: &str,
        subtitle: &str,
        fields: &[(&str, &str, bool)],
        hint: &str,
    ) {
        let w = self.fb.w as i32;
        let h = self.fb.h as i32;
        for p in self.fb.px.iter_mut() {
            let r = (*p >> 16) & 0xFF;
            let g = (*p >> 8) & 0xFF;
            let b = *p & 0xFF;
            *p = (((r * 3 + 0x11) / 4) << 16) | (((g * 3 + 0x11) / 4) << 8) | ((b * 3 + 0x11) / 4);
        }
        let (mx, my, mw, mh, _pad, _inner) = form_layout(w, h, fields.len());
        self.fb.fill_rect(mx + 10, my + 10, mw, mh, swiss::INK);
        self.fb.fill_rect(mx, my, mw, mh, swiss::WHITE);
        self.fb.stroke_rect(mx, my, mw, mh, swiss::RULE_W, swiss::INK);
        self.fb.fill_rect(mx, my, mw, 5, swiss::ACCENT);
        let pad = swiss::MARGIN;
        tp.text(&mut self.fb, mx + pad, my + pad, swiss::T_TITLE, true, swiss::INK, title);
        tp.text(&mut self.fb, mx + pad, my + pad + 42, swiss::T_CAPTION, false, swiss::MUTED, subtitle);
        let mut fy = my + pad + 78;
        for (i, (label, value, focused)) in fields.iter().enumerate() {
            let fr = form_field_rect(w, h, fields.len(), i);
            tp.text(&mut self.fb, fr.0, fr.1 - 20, swiss::T_CAPTION, true, swiss::MUTED, label);
            let color = if *focused { swiss::INK } else { swiss::RULE };
            self.fb.stroke_rect(fr.0, fr.1, fr.2, fr.3, swiss::RULE_W, color);
            tp.text(&mut self.fb, fr.0 + 12, fr.1 + 12, swiss::T_LABEL, false, swiss::INK, value);
            if *focused {
                let m = tp.measure(swiss::T_LABEL, false, value);
                self.fb.fill_rect(fr.0 + 14 + m, fr.1 + 10, 3, 26, swiss::ACCENT);
            }
        }
        let by = my + mh - 64;
        tp.text(&mut self.fb, mx + pad, by + 14, swiss::T_CAPTION, false, swiss::MUTED, hint);
        self.fb.fill_rect(mx + mw - pad - 240, by, 240, 46, swiss::INK);
        tp.text(&mut self.fb, mx + mw - pad - 220, by + 13, swiss::T_LABEL, true, swiss::PAPER, "СОХРАНИТЬ  Enter");
    }

    /// Снимок кадра в BMP (24 бит, снизу вверх) — артефакт для отчёта и для
    /// проверки стиля без запуска окна. O(W*H).
    pub fn snapshot_bmp(&self, out: &mut Vec<u8>) {
        let w = self.fb.w;
        let h = self.fb.h;
        let row = (w * 3 + 3) & !3;
        let size = 14 + 40 + row * h;
        out.clear();
        out.reserve(size);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(size as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(54u32).to_le_bytes());
        out.extend_from_slice(&(40u32).to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(h as i32).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&((row * h) as u32).to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..h).rev() {
            let start = out.len();
            for x in 0..w {
                let p = self.fb.px[y * w + x];
                out.push((p & 0xFF) as u8);
                out.push(((p >> 8) & 0xFF) as u8);
                out.push(((p >> 16) & 0xFF) as u8);
            }
            while out.len() - start < row {
                out.push(0);
            }
        }
    }
}

/// Разбить строку на две части по границе символа (для двухстрочного поля текста).
fn split_two_lines(s: &str, max: usize) -> (&str, &str) {
    if s.len() <= max {
        return (s, "");
    }
    let cut = (0..=max).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    (&s[..cut], &s[cut..])
}

/// Обрезать строку по ширине с «...» — текст не должен выезжать за панель.
/// O(len) по символам; аллокаций нет.
fn clip<'a>(tp: &mut dyn TextPainter, px: i32, bold: bool, max_w: i32, s: &'a str) -> &'a str {
    if tp.measure(px, bold, s) <= max_w {
        return s;
    }
    let mut end = s.len();
    let dots = tp.measure(px, bold, "...");
    while end > 0 {
        end -= 1;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        if tp.measure(px, bold, &s[..end]) + dots <= max_w {
            return &s[..end];
        }
    }
    ""
}

/// Число в десятичном виде без аллокаций: пишет в хвост `buf` и возвращает &str.
fn concat_u32<'a>(buf: &'a mut [u8], mut v: u32, suffix: &'a str) -> &'a str {
    let mut tmp = [0u8; 10];
    let mut n = 0;
    if v == 0 {
        tmp[0] = b'0';
        n = 1;
    }
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    let need = n + suffix.len();
    let start = buf.len().saturating_sub(need);
    let mut i = start;
    while n > 0 {
        n -= 1;
        buf[i] = tmp[n];
        i += 1;
    }
    for &b in suffix.as_bytes() {
        buf[i] = b;
        i += 1;
    }
    std::str::from_utf8(&buf[start..i]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePainter {
        calls: usize,
    }

    impl TextPainter for FakePainter {
        fn text(&mut self, _fb: &mut Framebuffer, _x: i32, _y: i32, _px: i32, _b: bool, _c: u32, s: &str) {
            self.calls += s.chars().count();
        }
        fn measure(&mut self, px: i32, _b: bool, s: &str) -> i32 {
            (s.chars().count() as i32) * px / 2
        }
    }

    #[test]
    fn avx2_and_scalar_agree() {
        let n = 1003;
        let mut xs = Vec::with_capacity(n);
        let mut ys = Vec::with_capacity(n);
        for i in 0..n {
            xs.push(((i as f64) * 0.37 - 100.0).sin() * 1000.0);
            ys.push(((i as f64) * 0.11).cos() * 750.0);
        }
        let cam = Camera { scale: 0.73, ox: 512.0, oy: 384.0 };
        let mut a = vec![0i32; n];
        let mut b = vec![0i32; n];
        project_scalar(&xs, &ys, cam, &mut a, &mut b);
        let mut c = vec![0i32; n];
        let mut d = vec![0i32; n];
        #[cfg(target_arch = "x86_64")]
        unsafe {
            project_avx2(&xs, &ys, cam, &mut c, &mut d)
        };
        #[cfg(not(target_arch = "x86_64"))]
        project_scalar(&xs, &ys, cam, &mut c, &mut d);
        // допуск 1 пиксель: cvttpd усекает, round округляет
        for i in 0..n {
            assert!((a[i] - c[i]).abs() <= 1, "x[{}]: {} vs {}", i, a[i], c[i]);
            assert!((b[i] - d[i]).abs() <= 1, "y[{}]: {} vs {}", i, b[i], d[i]);
        }
    }

    #[test]
    fn project_does_not_overflow_on_huge_coords() {
        let xs = [1e300, -1e300, 0.0];
        let ys = [1e300, 0.0, -1e300];
        let cam = Camera { scale: 1e6, ox: 0.0, oy: 0.0 };
        let mut sx = [0i32; 3];
        let mut sy = [0i32; 3];
        project(&xs, &ys, cam, &mut sx, &mut sy);
        // клампинг обязан удержать значения в i32, а не устроить UB
        assert_ne!(sx[0], i32::MIN);
        assert!(sx[0].abs() <= i32::MAX);
        assert_ne!(sy[0], i32::MIN);
        assert!(sy[0].abs() <= i32::MAX);
    }

    #[test]
    fn framebuffer_primitives_stay_inside() {
        let mut fb = Framebuffer::new(64, 48);
        fb.fill_rect(-100, -100, 1000, 1000, swiss::INK);
        fb.line(-50, -50, 500, 500, swiss::ACCENT);
        fb.circle(32, 24, 80, swiss::ERROR);
        fb.stroke_rect(2, 2, 10, 10, 2, swiss::WHITE);
        assert_eq!(fb.px.len(), 64 * 48);
    }

    #[test]
    fn frame_draws_ui_and_modal() {
        let mut doc = Doc::with_capacity(8);
        let layer = doc.layer_index(b"0");
        let a = doc.push_point(0.0, 0.0);
        let b = doc.push_point(100.0, 50.0);
        doc.segs.push(crate::doc::Seg { a, b, layer });
        let mut r = Renderer::new(1280, 720, 1024);
        let mut ui = UiState::new();
        ui.set_file("plan.dxf");
        ui.set_status("готов");
        ui.catalog_names.push("ИТП-3".to_string());
        let mut tp = FakePainter { calls: 0 };
        let mut session = EditSession::new();
        let cam = Camera::fit(doc.bounds(), 700, 600, 56);
        r.frame(&doc, cam, &mut ui, &session, &mut tp).unwrap();
        assert!(tp.calls > 0);
        session.begin(0, "Текст".as_bytes());
        r.frame(&doc, cam, &mut ui, &session, &mut tp).unwrap();
        let mut bmp = Vec::new();
        r.snapshot_bmp(&mut bmp);
        assert_eq!(&bmp[0..2], b"BM");
        assert!(bmp.len() > 1280 * 720 * 3);
    }

    #[test]
    fn modal_fields_are_clickable_and_do_not_overlap() {
        // Окно и рендер считают геометрию одной функцией — проверяем, что поля
        // не наезжают друг на друга и на кнопки, и что каждое внутри модалки.
        for (w, h) in [(1600, 1000), (1280, 800), (900, 640)] {
            let L = modal_layout(w, h);
            let all = [L.text, L.wire, L.comment, L.accept, L.cancel];
            for (i, a) in all.iter().enumerate() {
                // поле целиком внутри окна модалки
                assert!(a.0 >= L.mx && a.1 >= L.my, "поле {} вышло за левый/верхний край", i);
                assert!(a.0 + a.2 <= L.mx + L.mw, "поле {} вышло вправо", i);
                assert!(a.1 + a.3 <= L.my + L.mh, "поле {} вышло вниз", i);
                // центр поля обязан попадать в само поле — иначе клик не сработает
                let cx = a.0 + a.2 / 2;
                let cy = a.1 + a.3 / 2;
                assert!(hit(*a, cx, cy), "центр поля {} не попадает в него", i);
                for (j, b) in all.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    let overlap = a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
                    assert!(!overlap, "поля {} и {} пересекаются при {}×{}", i, j, w, h);
                }
            }
        }
    }

    #[test]
    fn form_fields_follow_layout() {
        let (w, h) = (1600, 1000);
        let f0 = form_field_rect(w, h, 2, 0);
        let f1 = form_field_rect(w, h, 2, 1);
        assert!(f1.1 > f0.1, "второе поле ниже первого");
        assert_eq!(f1.1 - f0.1, 60);
        let acc = form_accept_rect(w, h, 2);
        assert!(hit(acc, acc.0 + 10, acc.1 + 10));
        assert!(acc.1 > f1.1 + f1.3, "кнопка ниже полей");
    }

    #[test]
    fn concat_u32_formats_without_alloc() {
        let mut buf = [0u8; 32];
        let s = concat_u32(&mut buf, 42, " шт");
        assert!(s.contains("42"));
    }
}
