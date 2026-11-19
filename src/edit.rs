//! edit.rs — сценарий правки: единственный путь, которым изменение попадает в файл.
//!
//! Требование владельца, зашитое в автомат состояний: после правки утилита
//! спрашивает **номер провода**, и без него правка не применяется. Поэтому
//! «правка» здесь — не мутация документа, а заявка: текст держится в поле ввода,
//! документ не тронут, пока номер провода не принят. Отмена — правки нет вовсе
//! (это отличается от «применить, потом спросить»: там на диске уже лежит
//! изменение без трассировки, а на сетевом диске это недопустимо).
//!
//! Формат номера провода в ТЗ не задан. Принято значение по соглашению и вынесено
//! в конфиг (`require_wire`, `wire_pattern`): 1..=24 байта, первый символ —
//! буква/цифра, далее буквы, цифры и `. _ / -`. Примеры: `W-101`, `24A`, `Ш.3/7`.
//! Если у владельца есть своя маска (например, шифр линии по ГОСТ), она подменяет
//! эту одной строкой конфига.

use crate::doc::Doc;
use crate::text::EditBuffer;

/// Почему номер провода не принят. Показывается человеку в поле ввода.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    None,
    Empty,
    TooLong,
    BadChar,
}

impl WireError {
    pub fn message(self) -> &'static str {
        match self {
            WireError::None => "",
            WireError::Empty => "номер провода обязателен",
            WireError::TooLong => "номер провода: не больше 24 символов",
            WireError::BadChar => "номер провода: буквы, цифры и . _ / -",
        }
    }
}

pub const WIRE_MAX: usize = 24;

/// Проверка номера провода. O(len), len <= 24.
pub fn validate_wire(s: &[u8]) -> WireError {
    if s.is_empty() {
        return WireError::Empty;
    }
    if s.len() > WIRE_MAX {
        return WireError::TooLong;
    }
    for (i, &b) in s.iter().enumerate() {
        let ok = if b.is_ascii_alphanumeric() {
            true
        } else if b >= 0x80 {
            // кириллица в UTF-8: многобайтовые последовательности разрешены целиком
            true
        } else {
            matches!(b, b'.' | b'_' | b'/' | b'-' | b' ')
        };
        if !ok {
            return WireError::BadChar;
        }
        if i == 0 && !(b.is_ascii_alphanumeric() || b >= 0x80) {
            return WireError::BadChar;
        }
    }
    WireError::None
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Ничего не правится.
    Idle,
    /// Человек печатает новый текст надписи.
    Editing { idx: usize },
    /// Текст готов, ждём номер провода — документ ещё НЕ изменён.
    AwaitWire { idx: usize },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Applied,
    Canceled,
    Rejected,
    Nothing,
}

/// Состояние сценария правки. Живёт в главном цикле, памяти не растит.
pub struct EditSession {
    pub step: Step,
    pub text: EditBuffer,
    pub wire: EditBuffer,
    /// Свободный комментарий к правке (необязательный, но уходит в перечень).
    pub comment: EditBuffer,
    pub error: WireError,
    /// Последний принятый номер: подставляется как значение по умолчанию
    /// (в шлейфе провода номера идут серией, печатать заново — лишние движения).
    pub last_wire: EditBuffer,
    pub applied: u32,
    pub canceled: u32,
}

impl EditSession {
    pub const fn new() -> Self {
        Self {
            step: Step::Idle,
            text: EditBuffer::new(),
            wire: EditBuffer::new(),
            comment: EditBuffer::new(),
            error: WireError::None,
            last_wire: EditBuffer::new(),
            applied: 0,
            canceled: 0,
        }
    }

    #[inline]
    pub fn is_busy(&self) -> bool {
        !matches!(self.step, Step::Idle)
    }

    /// Шаг 1: начать правку надписи `idx` — поле ввода получает текущий текст.
    pub fn begin(&mut self, idx: usize, current: &[u8]) {
        self.text.set(current);
        self.wire.set(self.last_wire.as_bytes());
        self.wire.caret = self.wire.as_bytes().len();
        self.comment.set(b"");
        self.error = WireError::None;
        self.step = Step::Editing { idx };
    }

    /// Шаг 2: текст подтверждён -> запрос номера провода. Документ не тронут.
    pub fn commit_text(&mut self) -> Outcome {
        match self.step {
            Step::Editing { idx } => {
                self.step = Step::AwaitWire { idx };
                self.validate();
                Outcome::Nothing
            }
            _ => Outcome::Nothing,
        }
    }

    /// Шаг 3: номер провода введён -> правка применяется. Здесь и только здесь
    /// документ меняется, и в журнал уходит пара (old,new) вместе с номером.
    pub fn submit_wire(&mut self, doc: &mut Doc) -> Outcome {
        let idx = match self.step {
            Step::AwaitWire { idx } => idx,
            _ => return Outcome::Nothing,
        };
        self.validate();
        if self.error != WireError::None {
            return Outcome::Rejected;
        }
        let q = self.text.as_bytes().to_vec();
        let w = self.wire.as_bytes().to_vec();
        let c = self.comment.as_bytes().to_vec();
        doc.edit_text(idx, &q, &w, &c);
        let _ = self.last_wire.insert_char(' '); // no-op, чтобы не забыть про поле
        self.last_wire.set(&w);
        self.step = Step::Idle;
        self.applied += 1;
        Outcome::Applied
    }

    /// Отмена на любом шаге: правка не применяется, документ остаётся как был.
    pub fn cancel(&mut self) -> Outcome {
        if self.is_busy() {
            self.step = Step::Idle;
            self.canceled += 1;
            Outcome::Canceled
        } else {
            Outcome::Nothing
        }
    }

    fn validate(&mut self) {
        self.error = validate_wire(self.wire.as_bytes());
    }
}

/// Хелпер для тестов и для CLI: правка одним вызовом, с явным номером провода.
pub fn edit_with_wire(doc: &mut Doc, idx: usize, new: &[u8], wire: &[u8], comment: &[u8]) -> Outcome {
    if let WireError::None = validate_wire(wire) {
        doc.edit_text(idx, new, wire, comment);
        Outcome::Applied
    } else {
        Outcome::Rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Doc, TextEnt, TEXT_KIND_TEXT};
    use crate::pool::Handle;
    use crate::text::StrRef;

    fn doc() -> Doc {
        let mut d = Doc::with_capacity(8);
        let layer = d.layer_index(b"0");
        let p = d.push_point(0.0, 0.0);
        let (s, _) = d.arena.push("Клемма 1".as_bytes());
        d.texts.push(TextEnt {
            pos: p,
            height: 2.5,
            rot_deg: 0.0,
            layer,
            kind: TEXT_KIND_TEXT,
            s,
            wire: StrRef::EMPTY,
            dxf_handle: 0x10,
        });
        d.handles.push(Handle { slot: 0, gen: 1 });
        d
    }

    #[test]
    fn edit_is_not_applied_until_wire_is_given() {
        let mut d = doc();
        let mut s = EditSession::new();
        s.begin(0, &d.arena.get(d.texts[0].s).to_vec());
        s.text.set("Клемма 2".as_bytes());
        assert_eq!(s.commit_text(), Outcome::Nothing);
        // документ не тронут, пока номер провода не принят
        assert_eq!(d.arena.get(d.texts[0].s), "Клемма 1".as_bytes());
        assert_eq!(d.log.len(), 0);
        s.wire.set(b"W-101");
        assert_eq!(s.submit_wire(&mut d), Outcome::Applied);
        assert_eq!(d.arena.get(d.texts[0].s), "Клемма 2".as_bytes());
        assert_eq!(d.arena.get(d.texts[0].wire), "W-101".as_bytes());
        assert_eq!(d.log.len(), 1);
    }

    #[test]
    fn empty_wire_rejects_edit() {
        let mut d = doc();
        let mut s = EditSession::new();
        s.begin(0, b"x");
        s.text.set("y".as_bytes());
        s.commit_text();
        s.wire.set(b"");
        assert_eq!(s.submit_wire(&mut d), Outcome::Rejected);
        assert_eq!(s.error, WireError::Empty);
        assert_eq!(d.arena.get(d.texts[0].s), "Клемма 1".as_bytes());
        assert_eq!(d.log.len(), 0);
    }

    #[test]
    fn cancel_discards_edit() {
        let mut d = doc();
        let mut s = EditSession::new();
        s.begin(0, b"x");
        s.text.set("совсем другое".as_bytes());
        s.commit_text();
        assert_eq!(s.cancel(), Outcome::Canceled);
        assert_eq!(d.arena.get(d.texts[0].s), "Клемма 1".as_bytes());
        assert_eq!(s.canceled, 1);
    }

    #[test]
    fn comment_travels_with_the_edit() {
        let mut d = doc();
        let mut s = EditSession::new();
        s.begin(0, b"x");
        s.text.set("Клемма 9".as_bytes());
        s.comment.set("по замечанию ПТО".as_bytes());
        s.commit_text();
        s.wire.set(b"W-9");
        assert_eq!(s.submit_wire(&mut d), Outcome::Applied);
        let c = d.log.entries[0];
        assert_eq!(d.arena.get(c.comment), "по замечанию ПТО".as_bytes());
        assert_eq!(d.arena.get(c.wire), b"W-9");
    }

    #[test]
    fn last_wire_is_prefilled() {
        let mut d = doc();
        let mut s = EditSession::new();
        s.begin(0, b"x");
        s.text.set("a".as_bytes());
        s.commit_text();
        s.wire.set(b"W-7");
        assert_eq!(s.submit_wire(&mut d), Outcome::Applied);
        s.begin(0, b"a");
        assert_eq!(s.wire.as_bytes(), b"W-7");
    }

    #[test]
    fn wire_mask_rules() {
        assert_eq!(validate_wire(b"W-101"), WireError::None);
        assert_eq!(validate_wire(b"24A"), WireError::None);
        assert_eq!(validate_wire("Ш.3/7".as_bytes()), WireError::None);
        assert_eq!(validate_wire(b""), WireError::Empty);
        assert_eq!(validate_wire(b"-abc"), WireError::BadChar);
        assert_eq!(validate_wire(b"a#b"), WireError::BadChar);
        assert_eq!(validate_wire(b"aaaaaaaaaaaaaaaaaaaaaaaaa"), WireError::TooLong);
    }
}
