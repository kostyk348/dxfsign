//! win32.rs — системная типографика через GDI без единого крейта.
//!
//! Зачем GDI, а не свой шрифтовой движок: (1) Swiss Style требует Helvetica/Arial,
//! а на Windows 7 они есть в системе всегда, значит встраивать TTF и тащить
//! растеризатор не нужно; (2) GDI есть на Win7/Win8/Win10 без рантаймов, в отличие
//! от DirectWrite-обвязок; (3) Brutalism требует жёстких краёв — текст рисуется
//! в 1-битную маску, без сглаживания, и это стилистически верно, а не компромисс.
//!
//! Как это работает: GDI рисует строку чёрным по белому в маленький offscreen DIB,
//! затем ненулевые пиксели переносятся в наш кадр нужным цветом. Так `Framebuffer`
//! остаётся единственной поверхностью кадра, и снимок в BMP содержит и геометрию,
//! и надписи.
//!
//! Что НЕ реализовано в скелете: окно (CreateWindowEx + цикл сообщений). Причина
//! названа прямо: всё, кроме ввода с клавиатуры, проверяется снимком кадра;
//! ввод проверен тестами автомата `edit::EditSession`. Окно — тонкий презентер
//! поверх `Framebuffer`, его добавление не меняет ни модель, ни протокол сохранения.

#[cfg(windows)]
mod imp {
    use crate::render::{Framebuffer, TextPainter};
    use crate::text::utf8_to_cp1251;

    type Hdc = isize;
    type Hbitmap = isize;
    type Hgdiobj = isize;
    type Hfont = isize;

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

    #[repr(C)]
    struct Size {
        cx: i32,
        cy: i32,
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
        fn DeleteDC(hdc: Hdc) -> i32;
        fn CreateDIBSection(
            hdc: Hdc,
            pbmi: *const BitmapInfoHeader,
            usage: u32,
            ppv_bits: *mut *mut core::ffi::c_void,
            h_section: isize,
            offset: u32,
        ) -> Hbitmap;
        fn SelectObject(hdc: Hdc, obj: Hgdiobj) -> Hgdiobj;
        fn DeleteObject(obj: Hgdiobj) -> i32;
        fn CreateFontA(
            height: i32,
            width: i32,
            escapement: i32,
            orientation: i32,
            weight: i32,
            italic: u32,
            underline: u32,
            strike_out: u32,
            char_set: u32,
            out_precision: u32,
            clip_precision: u32,
            quality: u32,
            pitch_and_family: u32,
            face: *const u8,
        ) -> Hfont;
        fn SetTextColor(hdc: Hdc, color: u32) -> u32;
        fn SetBkColor(hdc: Hdc, color: u32) -> u32;
        fn SetBkMode(hdc: Hdc, mode: i32) -> i32;
        fn TextOutA(hdc: Hdc, x: i32, y: i32, s: *const u8, cch: i32) -> i32;
        fn GetTextExtentPoint32A(hdc: Hdc, s: *const u8, cch: i32, sz: *mut Size) -> i32;
    }

    const DIB_RGB_COLORS: u32 = 0;
    const TRANSPARENT: i32 = 1;
    const RUSSIAN_CHARSET: u32 = 204;
    const NONANTIALIASED_QUALITY: u32 = 1;
        const CLEARTYPE_QUALITY: u32 = 5;
    // Заготовка под моноширинную «типографскую» гарнитуру швейцарской школы:
    // Helvetica/Arial есть везде, дальше — то, что обычно стоит в цеху.
    const FACE: &[u8] = b"Arial\0";

    pub struct GdiPainter {
        mem: Hdc,
        bmp: Hbitmap,
        old_bmp: Hgdiobj,
        bits: *mut u32,
        tw: i32,
        th: i32,
        regular: Vec<(i32, Hfont)>,
        bold: Vec<(i32, Hfont)>,
        /// Счётчик вызовов: по нему видно, что кадр действительно рисовал текст.
        pub draws: u64,
        /// Переиспользуемый буфер CP1251: конвертация без аллокации на каждый вызов.
        cp: Vec<u8>,
        /// Кеш отрисованных строк. Надписи в чертежах повторяются («Изм.», «Лист»,
        /// «Подп. и дата»), а каждый промах — это два вызова GDI (measure + TextOutA).
        /// На Win7-машине 571 надпись × 2 вызова в кадре и давали «висит».
        glyphs: Vec<Glyph>,
        glyph_bytes: usize,
    }

    /// Готовая маска строки: 1 = закрасить, 0 = пропустить.
    struct Glyph {
        key: u64,
        px: i32,
        bold: bool,
        w: usize,
        rows: usize,
        mask: Vec<u8>,
    }

    /// Хеш строки: FNV-1a. Быстро, без аллокаций, без зависимостей.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        h
    }

    impl GdiPainter {
        pub fn new() -> Option<Self> {
            let tw = 1024;
            let th = 96;
            unsafe {
                let screen = CreateCompatibleDC(0);
                if screen == 0 {
                    return None;
                }
                let mut bih = BitmapInfoHeader {
                    bi_size: core::mem::size_of::<BitmapInfoHeader>() as u32,
                    bi_width: tw,
                    bi_height: -th, // top-down: строка 0 — верхняя, копировать проще
                    bi_planes: 1,
                    bi_bit_count: 32,
                    bi_compression: 0,
                    bi_size_image: (tw * th * 4) as u32,
                    ..Default::default()
                };
                let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
                let bmp = CreateDIBSection(
                    screen,
                    &bih,
                    DIB_RGB_COLORS,
                    &mut bits,
                    0,
                    0,
                );
                if bmp == 0 {
                    DeleteDC(screen);
                    return None;
                }
                let old_bmp = SelectObject(screen, bmp);
                SetBkMode(screen, TRANSPARENT);
                let _ = bih.bi_size;
                Some(Self {
                    mem: screen,
                    bmp,
                    old_bmp,
                    bits: bits as *mut u32,
                    tw,
                    th,
                    glyphs: Vec::with_capacity(64),
                    glyph_bytes: 0,
                    regular: Vec::with_capacity(4),
                    bold: Vec::with_capacity(4),
                    draws: 0,
                    cp: Vec::with_capacity(256),
                })
            }
        }

        fn font(&mut self, px: i32, bold: bool) -> Hfont {
            let cache = if bold { &mut self.bold } else { &mut self.regular };
            if let Some((_, f)) = cache.iter().find(|(p, _)| *p == px) {
                return *f;
            }
            unsafe {
                let f = CreateFontA(
                    -px, // отрицательная высота = высота символа (логические единицы)
                    0,
                    0,
                    0,
                    if bold { 700 } else { 400 },
                    0,
                    0,
                    0,
                    RUSSIAN_CHARSET,
                    0,
                    0,
                    NONANTIALIASED_QUALITY, // жёсткие края: и по стилю, и втрое быстрее
                    0,
                    FACE.as_ptr(),
                );
                cache.push((px, f));
                f
            }
        }

        /// Отрисовать строку в буфер (UTF-8 -> CP1251) и вернуть число байт.
        fn render_to_mask(&mut self, px: i32, bold: bool, s: &str, width: usize) -> usize {
            let mut cp = core::mem::take(&mut self.cp);
            utf8_to_cp1251(s.as_bytes(), &mut cp);
            let n = cp.len();
            if n > 200 {
                cp.truncate(200);
            }
            // Чистим только те строки, которые реально прочитаем. Раньше чистилось
            // tw*40 = 40 960 пикселей на КАЖДУЮ надпись: на чертеже с 571 надписью
            // это 23 млн записей в кадре, и на Win7-машине кадр вставал намертво.
            let rows = ((px * 2 + 8).max(16)).min(self.th) as usize;
            let clear_w = width.min(self.tw as usize).max(1);
            unsafe {
                // Чистим ровно тот прямоугольник, который потом прочитаем:
                // rows × ширина строки. Полная маска (1024×96) на каждую надпись —
                // это 98 тыс. записей, и на старом CPU кадр переставал рисоваться.
                for row in 0..rows {
                    let base = row * self.tw as usize;
                    for col in 0..clear_w {
                        *self.bits.add(base + col) = 0x00FF_FFFF;
                    }
                }
                let f = self.font(px, bold);
                SelectObject(self.mem, f);
                SetBkMode(self.mem, TRANSPARENT);
                SetBkColor(self.mem, 0x00FF_FFFF);
                SetTextColor(self.mem, 0x0000_0000);
                TextOutA(self.mem, 0, 0, cp.as_ptr(), cp.len() as i32);
            }
            self.cp = cp;
            n
        }
    }

    impl TextPainter for GdiPainter {
        fn text(&mut self, fb: &mut Framebuffer, x: i32, y: i32, px: i32, bold: bool, color: u32, s: &str) {
            if s.is_empty() {
                return;
            }
            // Попадание в кеш — только копирование памяти, без GDI.
            let key = fnv1a(s.as_bytes());
            let hit = self
                .glyphs
                .iter()
                .position(|g| g.key == key && g.px == px && g.bold == bold);
            if let Some(gi) = hit {
                let g = &self.glyphs[gi];
                blit_glyph(fb, g, x, y, color);
                self.draws += 1;
                return;
            }
            let width = self.measure(px, bold, s).max(0) as usize;
            if width == 0 || width > self.tw as usize {
                return;
            }
            self.render_to_mask(px, bold, s, width);
            let rows = ((px * 2 + 8).max(16)).min(self.th) as usize;
            let mut mask = vec![0u8; width * rows];
            for row in 0..rows {
                let base = row * self.tw as usize;
                for col in 0..width {
                    let p = unsafe { *self.bits.add(base + col) };
                    mask[row * width + col] = u8::from((p & 0xFF) < 0x80);
                }
            }
            // Кеш не растим бесконечно: 8 МБ хватает на тысячи разных строк.
            if self.glyph_bytes + mask.len() < 8 << 20 {
                self.glyph_bytes += mask.len();
                let g = Glyph { key, px, bold, w: width, rows, mask };
                blit_glyph(fb, &g, x, y, color);
                self.glyphs.push(g);
            } else {
                let g = Glyph { key, px, bold, w: width, rows, mask };
                blit_glyph(fb, &g, x, y, color);
            }
            self.draws += 1;
        }

        fn measure(&mut self, px: i32, bold: bool, s: &str) -> i32 {
            let mut cp = core::mem::take(&mut self.cp);
            utf8_to_cp1251(s.as_bytes(), &mut cp);
            let mut sz = Size { cx: 0, cy: 0 };
            unsafe {
                let f = self.font(px, bold);
                SelectObject(self.mem, f);
                GetTextExtentPoint32A(self.mem, cp.as_ptr(), cp.len() as i32, &mut sz);
            }
            self.cp = cp;
            sz.cx
        }
    }

    impl Drop for GdiPainter {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.mem, self.old_bmp);
                for (_, f) in self.regular.drain(..) {
                    DeleteObject(f);
                }
                for (_, f) in self.bold.drain(..) {
                    DeleteObject(f);
                }
                DeleteObject(self.bmp);
                DeleteDC(self.mem);
            }
        }
    }

    /// Перенести готовую маску строки в кадр. O(ширина × строки).
    fn blit_glyph(fb: &mut Framebuffer, g: &Glyph, x: i32, y: i32, color: u32) {
        for row in 0..g.rows {
            let base = row * g.w;
            for col in 0..g.w {
                if g.mask[base + col] != 0 {
                    fb.put(x + col as i32, y + row as i32, color);
                }
            }
        }
    }

    /// Заглушка-рисовальщик для машин без GDI (кросс-компиляция, тесты):
    /// меряет по средней ширине символа, чтобы раскладка не разъезжалась.
    pub struct ApproxPainter;
    impl TextPainter for ApproxPainter {
        fn text(&mut self, _fb: &mut Framebuffer, _x: i32, _y: i32, _px: i32, _b: bool, _c: u32, _s: &str) {}
        fn measure(&mut self, px: i32, _b: bool, s: &str) -> i32 {
            (s.chars().count() as i32 * px * 52) / 100
        }
    }
}

#[cfg(windows)]
pub use imp::{ApproxPainter, GdiPainter};

#[cfg(not(windows))]
pub struct ApproxPainter;

#[cfg(not(windows))]
impl crate::render::TextPainter for ApproxPainter {
    fn text(&mut self, _fb: &mut crate::render::Framebuffer, _x: i32, _y: i32, _px: i32, _b: bool, _c: u32, _s: &str) {}
    fn measure(&mut self, px: i32, _b: bool, s: &str) -> i32 {
        (s.chars().count() as i32 * px * 52) / 100
    }
}

#[cfg(not(windows))]
pub type GdiPainter = ApproxPainter;
