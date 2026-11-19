//! doc.rs — модель чертежа в DOP-стиле: Structure-of-Arrays, индексы, текст в арене.
//!
//! Что здесь принципиально:
//!  * Нет `Vec<Entity>` с enum-вариантами: сущности разложены по типам (SoA). Обход
//!    для отрисовки — плотный проход по массиву f64 без промахов по кэшу и без
//!    ветвления на теги вариантов.
//!  * Точки хранятся раздельно (`xs`/`ys`) — ровно тот вид, который грузится в AVX2
//!    четырьмя f64 за один `vmovupd`, без gather.
//!  * Ссылки между элементами — только `u32`-индексы. Ни указателей, ни `Rc`.
//!  * Текст — `StrRef` в `TextArena`: правка не перезаписывает старое значение,
//!    поэтому diff и хеш версии всегда имеют оба состояния.
//!
//! Единицы: DXF хранит координаты в единицах `$INSUNITS` (не задано в ТЗ — принимаем
//! «как в файле», без масштабирования; мировые координаты остаются f64).

use crate::pool::Handle;
use crate::text::{StrRef, TextArena};

/// Отрезок: индексы концов в массиве точек + слой.
#[derive(Clone, Copy, Debug, Default)]
pub struct Seg {
    pub a: u32,
    pub b: u32,
    pub layer: u16,
}

/// Окружность: индекс центра и радиус. Центр лежит в общей таблице точек.
#[derive(Clone, Copy, Debug, Default)]
pub struct Circle {
    pub c: u32,
    pub r: f64,
    pub layer: u16,
}

/// Полилиния: окно в `poly_verts` (вместо Vec<Vec<u32>> — одна аллокация на весь файл).
#[derive(Clone, Copy, Debug, Default)]
pub struct Poly {
    pub first: u32,
    pub len: u32,
    pub closed: bool,
    pub layer: u16,
}

pub const TEXT_KIND_TEXT: u8 = 0;
pub const TEXT_KIND_MTEXT: u8 = 1;

/// Текстовая сущность (DXF TEXT/MTEXT). Это главный объект правки.
#[derive(Clone, Copy, Debug, Default)]
pub struct TextEnt {
    pub pos: u32,
    pub height: f64,
    pub rot_deg: f64,
    pub layer: u16,
    pub kind: u8,
    pub s: StrRef,
    /// Номер провода, привязанный к правке этой надписи (XData DXFSIGN/WIRE).
    /// Хранится в арене, пишется в DXF как XData — трассировка живёт в файле.
    pub wire: StrRef,
    /// DXF handle сущности, если он был в файле (для round-trip и для лога правок).
    pub dxf_handle: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Layer {
    pub name: StrRef,
    pub color: u8,
}

/// Что именно изменилось — единица списка в `.txt`.
#[derive(Clone, Copy, Debug)]
pub struct Change {
    pub kind: ChangeKind,
    pub entity: Handle,
    pub old: StrRef,
    pub new: StrRef,
    /// Номер провода, введённый пользователем после правки (обязательный атрибут).
    pub wire: StrRef,
    /// Свободный комментарий к правке. Требование владельца: комментарий идёт
    /// в перечень предлагаемых изменений наравне с правкой. Может быть пустым.
    pub comment: StrRef,
    pub old_num: f64,
    pub new_num: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    TextEdited,
    TextMoved,
    TextHeight,
    LineMoved,
    LineAdded,
    LineDeleted,
    CircleEdited,
    PolyEdited,
    /// Комментарий без правки геометрии: замечание к чертежу, которое обязано
    /// попасть в перечень и в письмо хоть и не меняет ни одного байта геометрии.
    Comment,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::TextEdited => "TEXT",
            ChangeKind::TextMoved => "TEXT-POS",
            ChangeKind::TextHeight => "TEXT-HEIGHT",
            ChangeKind::LineMoved => "LINE",
            ChangeKind::LineAdded => "LINE+",
            ChangeKind::LineDeleted => "LINE-",
            ChangeKind::CircleEdited => "CIRCLE",
            ChangeKind::PolyEdited => "LWPOLYLINE",
            ChangeKind::Comment => "COMMENT",
        }
    }
}

/// Журнал правок текущей сессии. Ёмкость резервируется при открытии файла,
/// поэтому правка в главном цикле не аллоцирует (проверяется тестом аллокаций).
pub struct ChangeLog {
    pub entries: Vec<Change>,
    cap: usize,
}

impl ChangeLog {
    pub fn with_capacity(n: usize) -> Self {
        Self { entries: Vec::with_capacity(n), cap: n }
    }
    #[inline]
    pub fn push(&mut self, c: Change) -> bool {
        if self.entries.len() == self.cap {
            return false; // журнал переполнен: честнее сказать, чем молча потерять правку
        }
        self.entries.push(c);
        true
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Чертеж целиком.
pub struct Doc {
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
    pub segs: Vec<Seg>,
    pub circles: Vec<Circle>,
    pub polys: Vec<Poly>,
    pub poly_verts: Vec<u32>,
    pub texts: Vec<TextEnt>,
    pub handles: Vec<Handle>, // параллельно texts: стабильный адрес сущности для UI
    pub layers: Vec<Layer>,
    /// Имя слоя -> индекс. Без этой карты каждая сущность сканировала весь список
    /// слоёв (на реальных чертежах их сотни, сущностей — сотни тысяч).
    layer_map: std::collections::HashMap<Vec<u8>, u16>,
    pub arena: TextArena,
    pub log: ChangeLog,
    /// Путь рабочего файла (UTF-8, для лога и письма).
    pub path: String,
    /// DXF-версия файла, как прочитали (AC1009 и т.п.).
    pub dxf_version: String,
    /// Кодовая страница файла ($DWGCODEPAGE), нужна для записи в той же кодировке.
    pub codepage: String,
}

impl Doc {
    /// Резерв под `hint` сущностей: после этого открытие/правка не растят память
    /// на типичном файле, и кадр гарантированно проходит без malloc.
    pub fn with_capacity(hint: usize) -> Self {
        Self {
            xs: Vec::with_capacity(hint * 4),
            ys: Vec::with_capacity(hint * 4),
            segs: Vec::with_capacity(hint),
            circles: Vec::with_capacity(hint / 4),
            polys: Vec::with_capacity(hint / 4),
            poly_verts: Vec::with_capacity(hint * 4),
            texts: Vec::with_capacity(hint / 2),
            handles: Vec::with_capacity(hint / 2),
            layers: Vec::with_capacity(16),
            layer_map: std::collections::HashMap::with_capacity(64),
            arena: TextArena::with_capacity(hint * 24),
            log: ChangeLog::with_capacity(1024),
            path: String::with_capacity(260),
            dxf_version: String::with_capacity(8),
            codepage: String::with_capacity(16),
        }
    }

    pub fn clear(&mut self) {
        self.xs.clear();
        self.ys.clear();
        self.segs.clear();
        self.circles.clear();
        self.polys.clear();
        self.poly_verts.clear();
        self.texts.clear();
        self.handles.clear();
        self.layers.clear();
        self.layer_map.clear();
        self.log.clear();
        self.path.clear();
        self.dxf_version.clear();
        self.codepage.clear();
    }

    /// Срез текста из арены (для save/eml, где нужны сырые байты).
    #[inline]
    pub fn arena_get(&self, r: StrRef) -> &[u8] {
        self.arena.get(r)
    }

    /// Текст как строка для отчёта `.txt`. Потери кодировки заменяются символом
    /// замены U+FFFD, а не молча теряются.
    pub fn arena_utf8(&self, r: StrRef) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.arena.get(r))
    }

    #[inline]
    pub fn point(&self, i: u32) -> (f64, f64) {
        let i = i as usize;
        if i < self.xs.len() && i < self.ys.len() {
            (self.xs[i], self.ys[i])
        } else {
            (0.0, 0.0)
        }
    }

    #[inline]
    pub fn push_point(&mut self, x: f64, y: f64) -> u32 {
        let i = self.xs.len() as u32;
        self.xs.push(x);
        self.ys.push(y);
        i
    }

    /// Индекс слоя по имени. O(1) по хеш-карте (было O(L) с побайтовым сравнением).
    pub fn layer_index(&mut self, name: &[u8]) -> u16 {
        if let Some(i) = self.layer_map.get(name) {
            return *i;
        }
        if self.layers.len() >= u16::MAX as usize {
            return 0;
        }
        let (r, _) = self.arena.push(name);
        let idx = self.layers.len() as u16;
        self.layers.push(Layer { name: r, color: 7 });
        self.layer_map.insert(name.to_vec(), idx);
        idx
    }

    /// Сколько сущностей всех типов — для статусной строки и для оценки O.
    pub fn entity_count(&self) -> usize {
        self.segs.len() + self.circles.len() + self.polys.len() + self.texts.len()
    }

    /// Границы чертежа: O(N) по точкам, 4 независимых аккумулятора (DOP-дружелюбно).
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for i in 0..self.xs.len() {
            let (x, y) = (self.xs[i], self.ys[i]);
            if x < x0 {
                x0 = x;
            }
            if y < y0 {
                y0 = y;
            }
            if x > x1 {
                x1 = x;
            }
            if y > y1 {
                y1 = y;
            }
        }
        for c in &self.circles {
            let (cx, cy) = self.point(c.c);
            x0 = x0.min(cx - c.r);
            y0 = y0.min(cy - c.r);
            x1 = x1.max(cx + c.r);
            y1 = y1.max(cy + c.r);
        }
        if x0 > x1 {
            (0.0, 0.0, 1.0, 1.0)
        } else {
            (x0, y0, x1, y1)
        }
    }

    /// Канонический хеш состояния: проходит по SoA-массивам и текстам.
    /// Порядок фиксирован (типы, затем индексы), поэтому хеш воспроизводим и
    /// одинаков на разных машинах. O(N).
    pub fn hash(&self) -> [u8; 32] {
        use crate::sha256::Sha256;
        let mut h = Sha256::new();
        h.update(b"dxfsign/v1\0");
        h.update(&(self.xs.len() as u64).to_le_bytes());
        for i in 0..self.xs.len() {
            h.update(&self.xs[i].to_bits().to_le_bytes());
            h.update(&self.ys[i].to_bits().to_le_bytes());
        }
        for s in &self.segs {
            h.update(&s.a.to_le_bytes());
            h.update(&s.b.to_le_bytes());
            h.update(&s.layer.to_le_bytes());
        }
        for c in &self.circles {
            h.update(&c.c.to_le_bytes());
            h.update(&c.r.to_bits().to_le_bytes());
            h.update(&c.layer.to_le_bytes());
        }
        for p in &self.polys {
            h.update(&p.first.to_le_bytes());
            h.update(&p.len.to_le_bytes());
            h.update(&[p.closed as u8, p.layer as u8]);
        }
        for t in &self.texts {
            h.update(&t.pos.to_le_bytes());
            h.update(&t.height.to_bits().to_le_bytes());
            h.update(&t.rot_deg.to_bits().to_le_bytes());
            h.update(&t.layer.to_le_bytes());
            h.update(&[t.kind]);
            h.update(self.arena.get(t.s));
            h.update(b"\0");
            h.update(self.arena.get(t.wire));
            h.update(b"\0");
        }
        h.finish()
    }

    /// То же состояние, но коротким 64-битным слепком для быстрого «изменилось ли»:
    /// используется в кадре и при решении «писать версию или нет». O(N).
    pub fn quick_fingerprint(&self) -> u64 {
        let mut acc: u64 = 0xcbf29ce484222325;
        for i in 0..self.xs.len() {
            acc ^= self.xs[i].to_bits();
            acc = acc.wrapping_mul(0x100000001b3);
            acc ^= self.ys[i].to_bits();
            acc = acc.wrapping_mul(0x100000001b3);
        }
        for t in &self.texts {
            for &b in self.arena.get(t.s) {
                acc ^= b as u64;
                acc = acc.wrapping_mul(0x100000001b3);
            }
            acc ^= t.height.to_bits();
            acc = acc.wrapping_mul(0x100000001b3);
        }
        acc
    }

    /// Найти текстовую сущность по DXF handle (для правки «этой строки»).
    pub fn text_by_handle(&self, dxf_handle: u32) -> Option<usize> {
        self.texts.iter().position(|t| t.dxf_handle == dxf_handle)
    }

    /// Правка текста: новое значение уходит в арену, старое помечается мусором,
    /// в журнал пишется пара (old,new) — из неё потом собирается `.txt`.
    /// Возвращает false, если журнал переполнен (правка при этом уже применена —
    /// поэтому вызывающий обязан показать предупреждение).
    pub fn edit_text(&mut self, idx: usize, new: &[u8], wire: &[u8], comment: &[u8]) -> bool {
        if idx >= self.texts.len() {
            return false;
        }
        let old = self.texts[idx].s;
        if self.arena.get(old) == new {
            // текст не изменился — правки нет, но номер провода всё равно фиксируем
            // номер провода применяется и когда он пуст: иначе «вернуть как было»
            // не возвращает состояние, и хеш версии не совпадёт с исходным
            let (wr, _) = self.arena.push(wire);
            self.arena.mark_waste(self.texts[idx].wire);
            self.texts[idx].wire = wr;
            if !comment.is_empty() {
                return self.add_comment(idx, comment, wire);
            }
            return true;
        }
        let (nr, _) = self.arena.push(new);
        let (wr, _) = self.arena.push(wire);
        let (cr, _) = self.arena.push(comment);
        self.texts[idx].s = nr;
        self.arena.mark_waste(old);
        self.arena.mark_waste(self.texts[idx].wire);
        self.texts[idx].wire = wr;
        let h = self.handles[idx];
        self.log.push(Change {
            kind: ChangeKind::TextEdited,
            entity: h,
            old,
            new: nr,
            wire: wr,
            comment: cr,
            old_num: 0.0,
            new_num: 0.0,
        })
    }

    /// Комментарий свободного формата к надписи `idx`: попадает в перечень
    /// предлагаемых изменений, в `.txt` и в `.eml`, но геометрию не трогает.
    /// Номер провода здесь тоже обязателен: без него запись не привязывается
    /// к цепи, а именно привязка и есть смысл журнала.
    pub fn add_comment(&mut self, idx: usize, comment: &[u8], wire: &[u8]) -> bool {
        if idx >= self.texts.len() || comment.is_empty() {
            return true;
        }
        let (cr, _) = self.arena.push(comment);
        let (wr, _) = self.arena.push(wire);
        self.arena.mark_waste(self.texts[idx].wire);
        self.texts[idx].wire = wr;
        let h = self.handles[idx];
        let s = self.texts[idx].s;
        self.log.push(Change {
            kind: ChangeKind::Comment,
            entity: h,
            old: s,
            new: s,
            wire: wr,
            comment: cr,
            old_num: 0.0,
            new_num: 0.0,
        })
    }

    pub fn set_text_height(&mut self, idx: usize, h_new: f64) -> bool {
        if idx >= self.texts.len() {
            return false;
        }
        let old = self.texts[idx].height;
        if (old - h_new).abs() < f64::EPSILON {
            return true;
        }
        self.texts[idx].height = h_new;
        let h = self.handles[idx];
        let s = self.texts[idx].s;
        let w = self.texts[idx].wire;
        self.log.push(Change {
            kind: ChangeKind::TextHeight,
            entity: h,
            old: s,
            new: s,
            wire: w,
            comment: StrRef::EMPTY,
            old_num: old,
            new_num: h_new,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Doc {
        let mut d = Doc::with_capacity(16);
        let layer = d.layer_index(b"0");
        let a = d.push_point(0.0, 0.0);
        let b = d.push_point(10.0, 0.0);
        d.segs.push(Seg { a, b, layer });
        let c = d.push_point(5.0, 5.0);
        d.circles.push(Circle { c, r: 2.0, layer });
        let p = d.push_point(1.0, 1.0);
        let (s, _) = d.arena.push("Исходный текст".as_bytes());
        d.texts.push(TextEnt { pos: p, height: 2.5, rot_deg: 0.0, layer, kind: TEXT_KIND_TEXT, s, wire: StrRef::EMPTY, dxf_handle: 0x2A });
        d.handles.push(Handle { slot: 0, gen: 1 });
        d
    }

    #[test]
    fn bounds_include_circle_radius() {
        let d = doc();
        let (x0, y0, x1, y1) = d.bounds();
        assert!(x0 <= 0.0 && y0 <= 0.0);
        assert!(x1 >= 10.0 && y1 >= 7.0);
    }

    #[test]
    fn edit_text_keeps_old_value_for_diff() {
        let mut d = doc();
        let old_ref = d.texts[0].s;
        assert!(d.edit_text(0, "Новый текст".as_bytes(), b"W-101", "заменил по замечанию".as_bytes()));
        assert_eq!(d.arena.get(old_ref), "Исходный текст".as_bytes());
        assert_eq!(d.arena.get(d.texts[0].s), "Новый текст".as_bytes());
        assert_eq!(d.log.len(), 1);
        let ch = d.log.entries[0];
        assert_eq!(d.arena.get(ch.old), "Исходный текст".as_bytes());
        assert_eq!(d.arena.get(ch.new), "Новый текст".as_bytes());
        assert_eq!(d.arena.get(ch.wire), "W-101".as_bytes());
        assert_eq!(d.arena.get(ch.comment), "заменил по замечанию".as_bytes());
    }

    #[test]
    fn hash_changes_on_edit_and_restores_on_undo_of_value() {
        let mut d = doc();
        let h1 = d.hash();
        let h_before = d.hash();
        d.edit_text(0, "Другое".as_bytes(), b"W-102", b"");
        let h2 = d.hash();
        assert_ne!(h_before, h2);
        // Возврат к тому же тексту даёт другой хеш, если сменился номер провода —
        // провод это часть содержимого файла (XData), и это правильно фиксируется.
        d.edit_text(0, "Исходный текст".as_bytes(), b"W-102", b"");
        assert_ne!(h1, d.hash());
        d.edit_text(0, "Исходный текст".as_bytes(), b"", b"");
        assert_eq!(h1, d.hash(), "тот же текст и пустой провод = исходное состояние");
    }

    #[test]
    fn comment_only_entry_lands_in_log_without_touching_text() {
        let mut d = doc();
        let before = d.arena.get(d.texts[0].s).to_vec();
        assert!(d.add_comment(0, "проверить сечение по проекту".as_bytes(), b"W-5"));
        assert_eq!(d.arena.get(d.texts[0].s), before.as_slice());
        assert_eq!(d.log.len(), 1);
        let c = d.log.entries[0];
        assert_eq!(c.kind, ChangeKind::Comment);
        assert_eq!(d.arena.get(c.comment), "проверить сечение по проекту".as_bytes());
        assert_eq!(d.arena.get(c.wire), b"W-5");
    }

    #[test]
    fn changelog_is_bounded() {
        let mut d = doc();
        for i in 0..1100 {
            let ok = d.edit_text(0, format!("t{}", i).as_bytes(), b"W-1", b"");
            if i >= 1024 {
                assert!(!ok, "журнал обязан отказать, а не расти бесконечно");
            }
        }
        assert_eq!(d.log.len(), 1024);
    }
}
