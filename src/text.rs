//! text.rs — строки чертежа в DOP-стиле: байтовая арена + (offset,len) вместо String.
//!
//! Зачем арена: текстовых сущностей в чертеже десятки тысяч, каждая правка по String
//! означает malloc + memcpy + фрагментацию. Здесь текст лежит одним буфером, сущность
//! держит 8-байтовую ссылку, а правка дописывает новую строку в хвост (copy-on-write
//! для версий — старое значение остаётся целым для diff и для хеша).
//!
//! Кодировка: DXF до 2007 (AC1015-) пишет ANSI-кодом страницы $DWGCODEPAGE; для
//! русских файлов это CP1251. Внутри держим UTF-8. Полная CP1251-таблица не нужна:
//! 0xC0..0xFF отображается формулой, экзотика 0x80..0xBF — таблицей из 64 значений.

/// Ссылка на строку в арене. 8 байт, Copy, без владения.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StrRef {
    pub start: u32,
    pub len: u32,
}

impl StrRef {
    pub const EMPTY: StrRef = StrRef { start: 0, len: 0 };
    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Арена строк. Append-only; мусор считается и убирается `compact()`.
pub struct TextArena {
    bytes: Vec<u8>,
    waste: usize,
}

impl TextArena {
    pub fn with_capacity(n: usize) -> Self {
        Self { bytes: Vec::with_capacity(n), waste: 0 }
    }

    /// Дописать строку. Возвращает ссылку; `grew` = false, если ёмкости хватило.
    pub fn push(&mut self, s: &[u8]) -> (StrRef, bool) {
        let cap = self.bytes.capacity();
        let start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s);
        (StrRef { start, len: s.len() as u32 }, cap == self.bytes.capacity())
    }

    #[inline]
    pub fn get(&self, r: StrRef) -> &[u8] {
        let s = r.start as usize;
        let e = s + r.len as usize;
        if e <= self.bytes.len() {
            &self.bytes[s..e]
        } else {
            &[]
        }
    }

    /// Сколько байт занято мусором (перезаписанными строками).
    pub fn waste(&self) -> usize {
        self.waste
    }

    pub fn mark_waste(&mut self, r: StrRef) {
        self.waste += r.len as usize;
    }

    /// Сжать арену: живые ссылки переезжают в начало, `live` переписывается на месте.
    /// O(B log B) по числу живых ссылок; вызывать только вне главного цикла
    /// (при открытии/сохранении), не в кадре.
    pub fn compact(&mut self, live: &mut [StrRef]) {
        live.sort_unstable_by_key(|r| r.start);
        let mut out = Vec::with_capacity(self.bytes.len() - self.waste);
        let mut prev_end: u32 = 0;
        for r in live.iter_mut() {
            if r.len == 0 {
                continue;
            }
            let s = r.start as usize;
            let e = s + r.len as usize;
            if e > self.bytes.len() {
                *r = StrRef::EMPTY;
                continue;
            }
            // уже на месте и вплотную — копировать не нужно
            if out.len() as u32 == r.start && r.start >= prev_end {
                out.extend_from_slice(&self.bytes[s..e]);
            } else {
                let new_start = out.len() as u32;
                out.extend_from_slice(&self.bytes[s..e]);
                *r = StrRef { start: new_start, len: r.len };
            }
            prev_end = r.start + r.len;
        }
        self.bytes = out;
        self.waste = 0;
    }
}

// ---------------------------------------------------------------- CP1251 <-> UTF-8

static CP1251_HI: [u16; 64] = [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021,
        0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F,
        0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
        0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F,
        0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7,
        0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
        0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7,
        0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457
];

#[inline]
fn cp1251_to_char(b: u8) -> char {
    let cp = match b {
        0x00..=0x7F => b as u32,
        0xC0..=0xFF => 0x0410 + (b as u32 - 0xC0),
        0x80..=0xBF => CP1251_HI[(b - 0x80) as usize] as u32,
    };
    char::from_u32(cp).unwrap_or('\u{FFFD}')
}

#[inline]
fn char_to_cp1251(c: char) -> u8 {
    let cp = c as u32;
    match cp {
        0x00..=0x7F => cp as u8,
        0x0410..=0x044F => (cp - 0x0410 + 0xC0) as u8,
        _ => {
            let mut out = b'?';
            for (i, t) in CP1251_HI.iter().enumerate() {
                if *t as u32 == cp {
                    out = 0x80 + i as u8;
                    break;
                }
            }
            out
        }
    }
}

/// Байты CP1251 -> UTF-8 в переданный буфер. O(n), одна аллокация на весь вызов.
pub fn cp1251_to_utf8(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut buf = [0u8; 4];
    for &b in src {
        out.extend_from_slice(cp1251_to_char(b).encode_utf8(&mut buf).as_bytes());
    }
}

/// UTF-8 -> CP1251. Непредставимый символ становится '?', счётчик потерь отдаётся наружу,
/// чтобы вызывающий мог предупредить пользователя, а не потерять текст молча.
pub fn utf8_to_cp1251(src: &[u8], out: &mut Vec<u8>) -> usize {
    out.clear();
    let mut lost = 0;
    for ch in String::from_utf8_lossy(src).chars() {
        let b = char_to_cp1251(ch);
        if b == b'?' && ch != '?' {
            lost += 1;
        }
        out.push(b);
    }
    lost
}

// ---------------------------------------------------------------- поле ввода

/// Редактор одной строки фиксированной ёмкости: в кадре не аллоцирует вообще.
pub struct EditBuffer {
    buf: [u8; 256],
    len: usize,
    pub caret: usize,
    cap: usize,
}

impl EditBuffer {
    pub const fn new() -> Self {
        Self { buf: [0; 256], len: 0, caret: 0, cap: 256 }
    }

    pub fn set(&mut self, s: &[u8]) {
        let n = s.len().min(self.cap);
        self.buf[..n].copy_from_slice(&s[..n]);
        self.len = n;
        self.caret = n;
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Вставка символа в каретку. Возвращает false, если места нет (поле не растёт).
    pub fn insert_char(&mut self, ch: char) -> bool {
        let mut tmp = [0u8; 4];
        let s = ch.encode_utf8(&mut tmp).as_bytes();
        if self.len + s.len() > self.cap || self.caret > self.len {
            return false;
        }
        self.buf.copy_within(self.caret..self.len, self.caret + s.len());
        self.buf[self.caret..self.caret + s.len()].copy_from_slice(s);
        self.len += s.len();
        self.caret += s.len();
        true
    }

    pub fn backspace(&mut self) -> bool {
        if self.caret == 0 {
            return false;
        }
        // отступаем на границу UTF-8-символа
        let mut i = self.caret - 1;
        while i > 0 && (self.buf[i] & 0xC0) == 0x80 {
            i -= 1;
        }
        self.buf.copy_within(self.caret..self.len, i);
        self.len -= self.caret - i;
        self.caret = i;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp1251_roundtrip_cyrillic() {
        // «Проект 12» в CP1251
        let src: [u8; 9] = [0xCF, 0xF0, 0xEE, 0xE5, 0xEA, 0xF2, 0x20, 0x31, 0x32];
        let mut utf8 = Vec::new();
        cp1251_to_utf8(&src, &mut utf8);
        assert_eq!(std::str::from_utf8(&utf8).unwrap(), "Проект 12");
        let mut back = Vec::new();
        assert_eq!(utf8_to_cp1251(&utf8, &mut back), 0);
        assert_eq!(back, src.to_vec());
    }

    #[test]
    fn cp1251_special_range() {
        // 0x88 = €, 0xB9 = № по CP1251
        let mut out = Vec::new();
        cp1251_to_utf8(&[0x88, 0xB9], &mut out);
        assert_eq!(std::str::from_utf8(&out).unwrap(), "€№");
    }

    #[test]
    fn arena_compact_keeps_live() {
        let mut a = TextArena::with_capacity(64);
        let (r1, _) = a.push(b"STARAJA");
        let (r2, _) = a.push(b"NOVAYA");
        a.mark_waste(r1);
        let mut live = [r2];
        a.compact(&mut live);
        assert_eq!(a.get(live[0]), b"NOVAYA");
        assert_eq!(a.waste(), 0);
    }

    #[test]
    fn edit_buffer_utf8_safe() {
        let mut e = EditBuffer::new();
        e.set("Проект".as_bytes());
        assert!(e.backspace());
        assert_eq!(std::str::from_utf8(e.as_bytes()).unwrap(), "Проек");
        assert!(e.insert_char('т'));
        assert_eq!(std::str::from_utf8(e.as_bytes()).unwrap(), "Проект");
    }

    #[test]
    fn edit_buffer_rejects_overflow() {
        let mut e = EditBuffer::new();
        for _ in 0..256 {
            assert!(e.insert_char('a'));
        }
        assert!(!e.insert_char('b'));
        assert_eq!(e.as_bytes().len(), 256);
    }
}
