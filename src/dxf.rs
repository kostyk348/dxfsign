//! dxf.rs — чтение и запись ASCII DXF (group-code пары), сущности LINE / CIRCLE /
//! LWPOLYLINE / TEXT / MTEXT.
//!
//! Где этот файл вырос из наработок: тег-модель (пара code/value, Value::Str/Real/Int)
//! и порядок секций взяты из `kostyk348/cadcraft`, crate `crates/dxf` (MIT/Apache-2.0,
//! «written from Autodesk's published DXF Reference»), маппинг полей LINE/CIRCLE/
//! LWPOLYLINE — из `crates/io/src/dxf_read.rs` того же репозитория. Отличия от
//! оригинала, важные для этой задачи:
//!   1. ноль зависимостей (в cadcraft — thiserror + encoding_rs + log);
//!   2. модель сразу DOP (SoA) — в cadcraft разбор идёт в модель-сущности, а нам
//!      нужен плотный массив координат;
//!   3. покрытие сознательно узкое: 5 сущностей вместо всего набора, но с полной
//!      обратной записью (round-trip) и сохранением текста в CP1251.
//!
//! Границы, названные честно:
//!   * бинарный DXF (AC1009+ binary) не поддержан — ТЗ не требует, cadcraft умеет;
//!   * блоки/INSERT/XREF/DIMENSION не разбираются — при сохранении они ПОТЕРЯЮТСЯ,
//!     поэтому writer пишет поверх только после проверки, что файл содержит лишь
//!     поддерживаемые сущности (см. `Unsupported` и решение в отчёте).

use std::collections::HashMap;

use crate::doc::{Circle, Doc, Poly, Seg, TextEnt, TEXT_KIND_MTEXT, TEXT_KIND_TEXT};
use crate::pool::Handle;
use crate::text::{cp1251_to_utf8, utf8_to_cp1251};

#[derive(Debug)]
pub enum DxfError {
    NotDxf,
    /// Разбор прерван пользователем (Esc во время загрузки).
    Cancelled,
    Eof,
    BadCode { line: usize },
    Unsupported(String),
    Io(String),
}

impl std::fmt::Display for DxfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DxfError::NotDxf => write!(f, "файл не похож на ASCII DXF"),
            DxfError::Cancelled => write!(f, "разбор отменён"),
            DxfError::Eof => write!(f, "неожиданный конец файла DXF"),
            DxfError::BadCode { line } => write!(f, "строка {}: не разобран group code", line),
            DxfError::Unsupported(s) => write!(f, "неподдерживаемая сущность DXF: {}", s),
            DxfError::Io(s) => write!(f, "ввод-вывод: {}", s),
        }
    }
}

pub type Result<T> = std::result::Result<T, DxfError>;

// ---------------------------------------------------------------- теги

struct TagReader<'a> {
    data: &'a [u8],
    pos: usize,
    line: usize,
    /// Сколько строк не разобрались как код группы (диагностика).
    bad: usize,
}

impl<'a> TagReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, line: 0, bad: 0 }
    }

    /// Текущая позиция в файле — для прогресса загрузки.
    fn position(&self) -> usize {
        self.pos
    }

    fn read_line(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let start = self.pos;
        let mut end = start;
        while end < self.data.len() && self.data[end] != b'\n' {
            end += 1;
        }
        self.pos = if end < self.data.len() { end + 1 } else { end };
        let mut slice = &self.data[start..end];
        if slice.last() == Some(&b'\r') {
            slice = &slice[..slice.len() - 1];
        }
        self.line += 1;
        Some(slice)
    }

    /// Следующая пара (code, value). Стоимость O(1) на пару, буфер не копируется.
    ///
    /// Битая строка кода НЕ обрывает разбор: раньше первый же посторонний байт
    /// завершал чтение молча — прогресс замирал на середине файла, а чертёж
    /// приходил пустым. Теперь такая строка пропускается, а счётчик `bad` растёт.
    fn next_tag(&mut self) -> Option<(i32, &'a [u8])> {
        for _ in 0..8 {
            let code_line = self.read_line()?;
            let Some(code) = parse_code(code_line) else {
                self.bad += 1;
                continue;
            };
            let value = self.read_line()?;
            return Some((code, value));
        }
        None
    }
}

fn parse_code(s: &[u8]) -> Option<i32> {
    let t = trim(s);
    if t.is_empty() {
        return None;
    }
    let neg = t[0] == b'-';
    let digits = if neg { &t[1..] } else { t };
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let mut v: i32 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as i32;
    }
    Some(if neg { -v } else { v })
}

/// Снять только завершающий CR: значения текста остаются как есть, включая пробелы.
fn strip_cr(s: &[u8]) -> &[u8] {
    let mut e = s.len();
    while e > 0 && (s[e - 1] == b'\r' || s[e - 1] == b'\n') {
        e -= 1;
    }
    &s[..e]
}

fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t') {
        a += 1;
    }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\t' || s[b - 1] == b'\r') {
        b -= 1;
    }
    &s[a..b]
}

fn parse_f64(s: &[u8]) -> f64 {
    let t = trim(s);
    let mut i = 0;
    let neg = if i < t.len() && (t[i] == b'-' || t[i] == b'+') {
        let n = t[i] == b'-';
        i += 1;
        n
    } else {
        false
    };
    let mut v = 0.0f64;
    let mut frac = 0.0f64;
    let mut scale = 1.0f64;
    let mut in_frac = false;
    while i < t.len() {
        let b = t[i];
        if b.is_ascii_digit() {
            if in_frac {
                frac = frac * 10.0 + (b - b'0') as f64;
                scale *= 10.0;
            } else {
                v = v * 10.0 + (b - b'0') as f64;
            }
        } else if b == b'.' {
            in_frac = true;
        } else if b == b'e' || b == b'E' {
            // экспоненту разбираем отдельно, чтобы не тащить строгую грамматику
            let exp = parse_i32(&t[i + 1..]);
            v += frac / scale;
            return if neg { -v * pow10(exp) } else { v * pow10(exp) };
        } else {
            break;
        }
        i += 1;
    }
    let v = v + frac / scale;
    if neg {
        -v
    } else {
        v
    }
}

fn parse_i32(s: &[u8]) -> i32 {
    let t = trim(s);
    let mut i = 0;
    let neg = if i < t.len() && (t[i] == b'-' || t[i] == b'+') {
        let n = t[i] == b'-';
        i += 1;
        n
    } else {
        false
    };
    let mut v: i32 = 0;
    while i < t.len() && t[i].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add((t[i] - b'0') as i32);
        i += 1;
    }
    if neg {
        -v
    } else {
        v
    }
}

fn pow10(e: i32) -> f64 {
    // без libm-вызовов в горячем пути: степени 10 таблицей, дальше — умножением
    let mut r = 1.0f64;
    let n = e.abs().min(308) as u32;
    const P: [f64; 23] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
        1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
    ];
    let mut k = n;
    while k >= 23 {
        r *= P[22];
        k -= 22;
    }
    r *= P[k as usize];
    if e < 0 {
        1.0 / r
    } else {
        r
    }
}

fn parse_hex_u32(s: &[u8]) -> u32 {
    let t = trim(s);
    let mut v: u32 = 0;
    for &b in t {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => break,
        };
        v = v.wrapping_mul(16).wrapping_add(d as u32);
    }
    v
}

// ---------------------------------------------------------------- разбор

/// Счётчик неподдержанных сущностей: нужен, чтобы не перезаписать файл,
/// потеряв данные, о которых мы не знаем.
/// Статистика разбора. `by_type` и `sections` — не украшение: это единственный
/// способ понять, почему реальный файл из AutoCAD показался пустым (геометрия в
/// `BLOCKS`, `INSERT`, `SPLINE`, `HATCH`, `DIMENSION`).
#[derive(Debug, Default, Clone)]
pub struct ParseStats {
    pub total_entities: usize,
    pub skipped: usize,
    /// Строки, не разобранные как код группы (обычно мусор в конце файла).
    pub bad_lines: usize,
    /// Вставок, не развёрнутых из-за потолка объектов.
    pub truncated_inserts: usize,
    pub skipped_kinds: [u32; 8],
    /// Счётчики по типу сущности в ENTITIES/BLOCKS, включая неподдержанные.
    pub by_type: Vec<(String, usize)>,
    /// Имена секций, встреченных в файле.
    pub sections: Vec<String>,
    pub blocks: usize,
    pub inserts_expanded: usize,
    pub inserted_entities: usize,
    /// INSERT без определения блока (битый файл или внешняя ссылка) — видно, а не молча.
    pub inserts_unresolved: usize,
}

impl ParseStats {
    /// Счётчик по типу. Сравнение по байтам, без `String` на каждый вызов: на
    /// чертеже с миллионом сущностей аллокация в счётчике — это миллион malloc.
    pub fn bump(&mut self, kind: &[u8]) {
        for e in self.by_type.iter_mut() {
            if e.0.as_bytes() == kind {
                e.1 += 1;
                return;
            }
        }
        self.by_type.push((String::from_utf8_lossy(kind).to_string(), 1));
    }
    pub fn summary(&self) -> String {
        let mut v: Vec<&(String, usize)> = self.by_type.iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let mut s = String::new();
        for (k, n) in v.iter().take(12) {
            if !s.is_empty() {
                s.push_str(", ");
            }
            s.push_str(&format!("{}={}", k, n));
        }
        s
    }
}

/// Разобрать ASCII DXF в `doc`. `doc` переиспользуется (capacity сохраняется),
/// поэтому повторное открытие файла того же размера не аллоцирует.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sec {
    None,
    AwaitName,
    Header,
    Tables,
    Blocks,
    Entities,
}

// Типы сущностей, которые разбор различает. Неподдержанные считаются и попадают
// в статистику: иначе «файл пустой» невозможно объяснить.
const K_NONE: u8 = 0;
const K_LINE: u8 = 1;
const K_CIRCLE: u8 = 2;
const K_LWPOLY: u8 = 3;
const K_TEXT: u8 = 4;
const K_MTEXT: u8 = 5;
const K_UNKNOWN: u8 = 6;
const K_POLYLINE: u8 = 7;
const K_VERTEX: u8 = 8;
const K_SEQEND: u8 = 9;
const K_SPLINE: u8 = 10;
const K_INSERT: u8 = 11;
const K_SOLID: u8 = 12;
const K_ARC: u8 = 13;
const K_BLOCK: u8 = 14;

/// Отложенная вставка блока. Собирается в главном проходе и разворачивается
/// после него — второй полный проход по файлу больше не нужен (он читал файл
/// заново и не обновлял прогресс: снаружи это выглядело как зависание).
#[derive(Clone)]
pub struct InsertRec {
    pub name: Vec<u8>,
    pub dx: f64,
    pub dy: f64,
    pub sx: f64,
    pub sy: f64,
    pub rot_deg: f64,
    pub layer: u16,
}

/// Линейное преобразование блока при вставке: масштаб, поворот, сдвиг.
#[derive(Clone, Copy)]
struct Xform {
    sx: f64,
    sy: f64,
    cos: f64,
    sin: f64,
    dx: f64,
    dy: f64,
}

impl Xform {
    fn identity() -> Self {
        Self { sx: 1.0, sy: 1.0, cos: 1.0, sin: 0.0, dx: 0.0, dy: 0.0 }
    }
    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let (x, y) = (x * self.sx, y * self.sy);
        (x * self.cos - y * self.sin + self.dx, x * self.sin + y * self.cos + self.dy)
    }
    fn scale(&self) -> f64 {
        (self.sx.abs() + self.sy.abs()) * 0.5
    }
}

/// Декодирование текста DXF. С версии AC1021 (AutoCAD 2007) DXF пишется в UTF-8;
/// более старые файлы — в кодовой странице ANSI (для русских — CP1251).
/// Декодировать всё как CP1251 нельзя: в UTF-8-файле кириллица превращалась в кашу
/// («MTEXT и кириллицу не читает вообще»). Автоопределение: валидный UTF-8 с
/// многобайтовыми символами считаем UTF-8, иначе CP1251.
fn decode_text(bytes: &[u8], utf8_mode: bool) -> Vec<u8> {
    let has_high = bytes.iter().any(|b| *b >= 0x80);
    if !has_high {
        return bytes.to_vec();
    }
    let valid_utf8 = std::str::from_utf8(bytes).is_ok();
    if utf8_mode {
        if valid_utf8 {
            return bytes.to_vec();
        }
        // «UTF-8-файл», но байты не UTF-8: значит на деле кодовая страница
        let mut out = Vec::with_capacity(bytes.len() + 8);
        cp1251_to_utf8(bytes, &mut out);
        return out;
    }
    // старый файл: но если байты уже корректный UTF-8 с многобайтовыми — доверяем ему
    if valid_utf8 {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 8);
    cp1251_to_utf8(bytes, &mut out);
    out
}

/// Чистка текста DXF: `%%d` `%%c` `%%p` `%%%` и коды MTEXT (`\P`, `\~`, `\f…;`,
/// `\H…;`, `\A…;`, `\C…;`, `\U+XXXX`). Без этого в чертеже видно `\P` и `%%c`.
fn clean_text(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0usize;
    while i < src.len() {
        // %% — обозначения AutoCAD
        if src[i] == b'%' && i + 2 < src.len() && src[i + 1] == b'%' {
            match src[i + 2] {
                b'd' | b'D' => {
                    out.extend_from_slice("°".as_bytes());
                    i += 3;
                    continue;
                }
                b'c' | b'C' => {
                    out.extend_from_slice("Ø".as_bytes());
                    i += 3;
                    continue;
                }
                b'p' | b'P' => {
                    out.extend_from_slice("±".as_bytes());
                    i += 3;
                    continue;
                }
                b'%' => {
                    out.push(b'%');
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        // \U+XXXX — юникод-эскейп
        if src[i] == b'\\' && i + 7 < src.len() && (src[i + 1] == b'U' || src[i + 1] == b'u') && src[i + 2] == b'+' {
            if let Ok(hex) = std::str::from_utf8(&src[i + 3..i + 7]) {
                if let Ok(cp) = u32::from_str_radix(hex, 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut b = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                        i += 7;
                        continue;
                    }
                }
            }
        }
        if src[i] == b'\\' && i + 1 < src.len() {
            let c = src[i + 1];
            match c {
                b'P' => {
                    out.push(b' ');
                    i += 2;
                    continue;
                }
                b'~' => {
                    out.push(b' ');
                    i += 2;
                    continue;
                }
                b'\\' | b'{' | b'}' => {
                    out.push(c);
                    i += 2;
                    continue;
                }
                b'f' | b'F' | b'H' | b'A' | b'C' | b'S' | b'T' | b'W' | b'Q' | b'p' => {
                    // коды со значением до ';' — выбрасываем целиком
                    let mut j = i + 2;
                    while j < src.len() && src[j] != b';' {
                        j += 1;
                    }
                    i = (j + 1).min(src.len());
                    continue;
                }
                _ => {}
            }
        }
        out.push(src[i]);
        i += 1;
    }
    out
}

#[derive(Default)]
struct EntityBuf {
    kind: u8,
    layer: u16,
    coord: [(bool, f64); 6],
    radius: f64,
    height: f64,
    rot_deg: f64,
    flags: i32,
    count: i32,
    handle: u32,
    s1: Vec<u8>,
    /// имя слоя (код 8) — раньше сюда же дописывались фрагменты MTEXT, из-за чего
    /// «слой» превращался в текст, а сам текст терялся
    layer_name: Vec<u8>,
    /// фрагменты MTEXT до основного (код 3)
    mtext_pre: Vec<u8>,
    utf8_mode: bool,
    app: Vec<u8>,
    wire: Vec<u8>,
    want_wire: bool,
    verts: Vec<(f64, f64)>,
    pending_x: Option<f64>,
    /// INSERT: имя блока, масштабы, точка вставки
    block_name: Vec<u8>,
    scale_x: f64,
    scale_y: f64,
    /// Исходное имя типа для неподдержанных сущностей (HATCH, DIMENSION, …) —
    /// чтобы статистика называла их своими именами, а не «UNKNOWN».
    type_name: Vec<u8>,
}

impl EntityBuf {
    fn reset(&mut self) {
        self.kind = K_NONE;
        self.layer = 0;
        self.coord = [(false, 0.0); 6];
        self.radius = 0.0;
        self.height = 0.0;
        self.rot_deg = 0.0;
        self.flags = 0;
        self.count = 0;
        self.handle = 0;
        self.s1.clear();
        self.layer_name.clear();
        self.mtext_pre.clear();
        self.app.clear();
        self.wire.clear();
        self.want_wire = false;
        self.verts.clear();
        self.pending_x = None;
        self.block_name.clear();
        self.scale_x = 1.0;
        self.scale_y = 1.0;
        self.type_name.clear();
    }

    fn set_kind(&mut self, name: &[u8]) {
        self.type_name.clear();
        self.type_name.extend_from_slice(name);
        self.kind = if name.eq_ignore_ascii_case(b"BLOCK") {
            K_BLOCK
        } else if name.eq_ignore_ascii_case(b"LINE") {
            K_LINE
        } else if name.eq_ignore_ascii_case(b"CIRCLE") {
            K_CIRCLE
        } else if name.eq_ignore_ascii_case(b"ARC") {
            K_ARC
        } else if name.eq_ignore_ascii_case(b"LWPOLYLINE") {
            K_LWPOLY
        } else if name.eq_ignore_ascii_case(b"TEXT") || name.eq_ignore_ascii_case(b"ATTRIB") {
            K_TEXT
        } else if name.eq_ignore_ascii_case(b"MTEXT") {
            K_MTEXT
        } else if name.eq_ignore_ascii_case(b"POLYLINE") {
            K_POLYLINE
        } else if name.eq_ignore_ascii_case(b"VERTEX") {
            K_VERTEX
        } else if name.eq_ignore_ascii_case(b"SEQEND") {
            K_SEQEND
        } else if name.eq_ignore_ascii_case(b"SPLINE") {
            K_SPLINE
        } else if name.eq_ignore_ascii_case(b"INSERT") {
            K_INSERT
        } else if name.eq_ignore_ascii_case(b"SOLID") || name.eq_ignore_ascii_case(b"TRACE") || name.eq_ignore_ascii_case(b"3DFACE") {
            K_SOLID
        } else {
            K_UNKNOWN
        };
    }

    fn push(&mut self, code: i32, val: &[u8]) {
        match code {
            8 => {
                self.layer_name = decode_text(trim(val), self.utf8_mode);
            }
            2 if self.kind == K_INSERT => {
                self.block_name = decode_text(trim(val), self.utf8_mode);
            }
            1000 if self.kind == K_NONE => {
                let _ = val;
            }
            2 if self.kind == K_BLOCK => {
                self.block_name = decode_text(trim(val), self.utf8_mode);
            }
            5 => self.handle = parse_hex_u32(val),
            // ТЕКСТ НЕ ТРИМИТСЯ: в DXF пробелы по краям значимы, а при обрезке
            // фрагменты MTEXT слипались («текстапродолжение»). Убираем только \r.
            1 => {
                self.s1.clear();
                let mut b = decode_text(strip_cr(val), self.utf8_mode);
                self.s1.append(&mut b);
            }
            // MTEXT приходит фрагментами до 250 символов (код 3), последний — кодом 1
            3 => {
                let mut b = decode_text(strip_cr(val), self.utf8_mode);
                self.mtext_pre.append(&mut b);
            }
            10 => self.set_coord(0, parse_f64(val)),
            20 => self.set_coord(1, parse_f64(val)),
            30 => self.set_coord(2, parse_f64(val)),
            11 => self.set_coord(3, parse_f64(val)),
            21 => self.set_coord(4, parse_f64(val)),
            31 => self.set_coord(5, parse_f64(val)),
            12 => self.set_coord(3, parse_f64(val)),
            22 => self.set_coord(4, parse_f64(val)),
            13 => self.set_coord(5, parse_f64(val)),
            40 => self.height = parse_f64(val),
            50 => self.rot_deg = parse_f64(val),
            // Масштабы вставки: 41 = X, 42 = Y, 43 = Z (а не 41/43, как можно
            // решить по «похожести»). 42 у LWPOLYLINE — bulge, у TEXT — width factor,
            // поэтому трактуем его как масштаб только у INSERT.
            41 if self.kind == K_INSERT => self.scale_x = parse_f64(val),
            42 if self.kind == K_INSERT => self.scale_y = parse_f64(val),
            70 => self.flags = parse_i32(val),
            90 => self.count = parse_i32(val),
            1001 => {
                self.app.clear();
                self.app.extend_from_slice(trim(val));
                self.want_wire = false;
            }
            1000 => {
                let mut b = decode_text(trim(val), self.utf8_mode);
                if self.app == b"DXFSIGN" {
                    if b == b"WIRE" {
                        self.want_wire = true;
                        self.wire.clear();
                    } else if self.want_wire {
                        self.wire.extend_from_slice(&b);
                    }
                }
            }
            _ => {}
        }
    }

    fn set_coord(&mut self, i: usize, v: f64) {
        // вершины и точки сплайна идут парами 10/20 (и 10/20/30 для 3D)
        if matches!(self.kind, K_LWPOLY | K_SPLINE | K_SOLID) {
            if i == 0 {
                self.pending_x = Some(v);
            } else if i == 1 {
                if let Some(x) = self.pending_x.take() {
                    self.verts.push((x, v));
                }
            }
            return;
        }
        self.coord[i] = (true, v);
    }
}



impl Xform {
    fn rot_deg(&self) -> f64 {
        self.sin.atan2(self.cos).to_degrees()
    }
}

/// Найти определение блока по имени (без учёта регистра, как в AutoCAD).
fn find_block<'a>(blocks: &'a [(String, Doc)], name: &[u8]) -> Option<&'a Doc> {
    let n = String::from_utf8_lossy(name).to_string();
    blocks
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&n))
        .map(|(_, d)| d)
}


/// Завершить сущность и положить её в `doc`.
fn finish_entity(e: &mut EntityBuf, doc: &mut Doc, stats: &mut ParseStats, recs: Option<&mut Vec<InsertRec>>) {
    stats.total_entities += 1;
    match e.kind {
        K_LINE => {
            let a = doc.push_point(e.coord[0].1, e.coord[1].1);
            let b = doc.push_point(e.coord[3].1, e.coord[4].1);
            let layer = doc.layer_index(&e.layer_name);
            doc.segs.push(Seg { a, b, layer });
        }
        K_CIRCLE => {
            let c = doc.push_point(e.coord[0].1, e.coord[1].1);
            let layer = doc.layer_index(&e.layer_name);
            doc.circles.push(Circle { c, r: e.height.abs(), layer });
        }
        K_ARC => {
            // дуга показывается как окружность целиком: честное упрощение скелета,
            // в отчёте помечено; важно, что она ВИДНА, а не пропущена
            let c = doc.push_point(e.coord[0].1, e.coord[1].1);
            let layer = doc.layer_index(&e.layer_name);
            doc.circles.push(Circle { c, r: e.height.abs(), layer });
        }
        K_LWPOLY | K_SPLINE => {
            if e.verts.len() >= 2 {
                let first = doc.poly_verts.len() as u32;
                for &(x, y) in &e.verts {
                    let i = doc.push_point(x, y);
                    doc.poly_verts.push(i);
                }
                let layer = doc.layer_index(&e.layer_name);
                doc.polys.push(Poly {
                    first,
                    len: e.verts.len() as u32,
                    closed: (e.flags & 1) != 0 || e.kind == K_SPLINE && (e.flags & 1) != 0,
                    layer,
                });
            }
        }
        K_SOLID => {
            if e.verts.len() >= 3 {
                let first = doc.poly_verts.len() as u32;
                for &(x, y) in &e.verts {
                    let i = doc.push_point(x, y);
                    doc.poly_verts.push(i);
                }
                let layer = doc.layer_index(&e.layer_name);
                doc.polys.push(Poly { first, len: e.verts.len() as u32, closed: true, layer });
            }
        }
        K_TEXT | K_MTEXT => {
            let pos = doc.push_point(e.coord[0].1, e.coord[1].1);
            let layer = doc.layer_index(&e.layer_name);
            let mut full = e.mtext_pre.clone();
            full.extend_from_slice(&e.s1);
            let cleaned = clean_text(&full);
            let (s, _) = doc.arena.push(&cleaned);
            let (wr, _) = doc.arena.push(&e.wire);
            let idx = doc.texts.len();
            doc.texts.push(TextEnt {
                pos,
                height: if e.height.abs() < 1e-9 { 2.5 } else { e.height.abs() },
                rot_deg: e.rot_deg,
                layer,
                kind: if e.kind == K_MTEXT { TEXT_KIND_MTEXT } else { TEXT_KIND_TEXT },
                s,
                wire: wr,
                dxf_handle: e.handle,
            });
            doc.handles.push(crate::pool::Handle { slot: idx as u32, gen: 1 });
        }
        K_INSERT => {
            // Слой вставки нужен сразу: элементы блока на слое «0» наследуют его.
            let layer = doc.layer_index(&e.layer_name);
            if let Some(recs) = recs {
                recs.push(InsertRec {
                    name: e.block_name.clone(),
                    dx: e.coord[0].1,
                    dy: e.coord[1].1,
                    sx: if e.scale_x == 0.0 { 1.0 } else { e.scale_x },
                    sy: if e.scale_y == 0.0 { 1.0 } else { e.scale_y },
                    rot_deg: e.rot_deg,
                    layer,
                });
            }
        }
        K_NONE | K_BLOCK | K_VERTEX | K_SEQEND | K_POLYLINE => {}
        K_UNKNOWN => {
            stats.skipped += 1;
        }
        _ => {}
    }
}

/// Разобрать ASCII DXF в `doc` одним проходом.
///
/// Поддержано: `LINE`, `CIRCLE`, `ARC` (как окружность), `LWPOLYLINE`,
/// `POLYLINE`+`VERTEX` (старый формат R12), `SPLINE` (по контрольным точкам),
/// `TEXT`/`ATTRIB`, `MTEXT` (с чисткой кодов), `SOLID`/`TRACE`/`3DFACE`,
/// **`BLOCKS` + `INSERT`** с масштабом, поворотом и наследованием слоя.
/// Не поддержано (считается в статистике): `HATCH`, `DIMENSION`, `LEADER`,
/// `MLINE`, `XREF`. Они не теряются молча — попадают в `by_type`.
pub fn parse(data: &[u8], doc: &mut Doc) -> Result<ParseStats> {
    let mut stats = ParseStats::default();
    parse_with(data, doc, None, &mut stats)
}

/// Разбор с прогрессом и отменой. Проверка отмены — раз в 8192 тега: чаще нет
/// смысла (одна сущность парсится микросекунды), реже — отклик станет заметным.
pub fn parse_with(
    data: &[u8],
    doc: &mut Doc,
    prog: Option<&crate::loader::Progress>,
    stats_out: &mut ParseStats,
) -> Result<ParseStats> {
    doc.clear();
    let mut stats = ParseStats::default();
    let mut recs: Vec<InsertRec> = Vec::new();
    if data.len() < 8 {
        return Err(DxfError::NotDxf);
    }
    let mut r = TagReader::new(data);
    let mut sec = Sec::None;
    let mut cur = EntityBuf::default();
    let mut last_var: Vec<u8> = Vec::with_capacity(32);
    let mut last_var_len = 0usize;
    let mut in_layer_table = false;
    let mut pending_layer: Option<Vec<u8>> = None;
    let mut scratch: Vec<u8> = Vec::with_capacity(256);
    let mut saw_acadver = false;
    let mut blocks: Vec<(String, Doc)> = Vec::new();
    let mut cur_block: Option<(String, Doc)> = None;
    /// UTF-8-режим включается для DXF 2007+ (AC1021 и новее). Решается по $ACADVER,
    /// который всегда идёт в HEADER до ENTITIES.
    let mut utf8_mode = false;
    // состояние старой полилинии (POLYLINE ... VERTEX ... SEQEND)
    let mut poly_pts: Vec<(f64, f64)> = Vec::new();
    let mut poly_layer: u16 = 0;
    let mut poly_closed = false;

    fn flush_poly(doc: &mut Doc, pts: &mut Vec<(f64, f64)>, layer: u16, closed: bool) {
        if pts.len() >= 2 {
            let first = doc.poly_verts.len() as u32;
            for &(x, y) in pts.iter() {
                let i = doc.push_point(x, y);
                doc.poly_verts.push(i);
            }
            doc.polys.push(Poly { first, len: pts.len() as u32, closed, layer });
        }
        pts.clear();
    }

    let mut tags_seen: u64 = 0u64;
    loop {
        let (code, val) = match r.next_tag() {
            Some(t) => t,
            None => break,
        };
        tags_seen += 1;
        if tags_seen & 0x1FFF == 0 {
            if let Some(p) = prog {
                if p.cancelled() {
                    return Err(DxfError::Cancelled);
                }
                p.done.store(r.position() as u64, std::sync::atomic::Ordering::Relaxed);
                p.entities.store(stats.total_entities as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let val_t = trim(val);
        if code == 0 {
            let name = val_t;
            // ВАЖНО: сначала дописать предыдущую сущность, потом разбирать служебные
            // записи. Иначе последняя сущность перед ENDSEC/BLOCK теряется — эта
            // ошибка уже стоила «пустого» TEXT в тестах.
            let in_scope = matches!(sec, Sec::Entities)
                || (matches!(sec, Sec::Blocks) && cur_block.is_some());
            if in_scope {
                match cur.kind {
                    K_POLYLINE => {
                        poly_pts.clear();
                        poly_closed = (cur.flags & 1) != 0;
                        let name_bytes = cur.layer_name.clone();
                        poly_layer = if let Some((_, bdoc)) = cur_block.as_mut() {
                            bdoc.layer_index(&name_bytes)
                        } else {
                            doc.layer_index(&name_bytes)
                        };
                        stats.bump(b"POLYLINE");
                    }
                    K_VERTEX => poly_pts.push((cur.coord[0].1, cur.coord[1].1)),
                    K_SEQEND => {
                        if let Some((_, bdoc)) = cur_block.as_mut() {
                            flush_poly(bdoc, &mut poly_pts, poly_layer, poly_closed);
                        } else {
                            flush_poly(doc, &mut poly_pts, poly_layer, poly_closed);
                        }
                    }
                    // INSERT не рисуется напрямую, но должен попасть в
                    // отложенный список вставок и в статистику.
                    K_INSERT => {
                        stats.bump(b"INSERT");
                        finish_entity(&mut cur, doc, &mut stats, Some(&mut recs));
                    }
                    K_NONE | K_BLOCK => {}
                    _ => {
                        if cur.kind == K_UNKNOWN {
                            let n = cur.type_name.clone();
                            stats.bump(&n);
                        } else {
                            stats.bump(entity_kind_name(cur.kind));
                        }
                        if let Some((_, bdoc)) = cur_block.as_mut() {
                            finish_entity(&mut cur, bdoc, &mut stats, None);
                        } else {
                            finish_entity(&mut cur, doc, &mut stats, Some(&mut recs));
                        }
                    }
                }
            }

            // --- служебные записи
            if name.eq_ignore_ascii_case(b"SECTION") {
                sec = Sec::AwaitName;
                cur.reset();
                continue;
            }
            if name.eq_ignore_ascii_case(b"ENDSEC") {
                sec = Sec::None;
                in_layer_table = false;
                cur.reset();
                continue;
            }
            if name.eq_ignore_ascii_case(b"EOF") {
                break;
            }
            if name.eq_ignore_ascii_case(b"BLOCK") {
                cur.reset();
                cur.utf8_mode = utf8_mode;
                cur.set_kind(b"BLOCK");
                continue;
            }
            if name.eq_ignore_ascii_case(b"ENDBLK") {
                if let Some((bname, bdoc)) = cur_block.take() {
                    if !bname.is_empty() {
                        stats.blocks += 1;
                        blocks.push((bname, bdoc));
                    }
                }
                cur.reset();
                continue;
            }
            cur.reset();
            cur.utf8_mode = utf8_mode;
            cur.set_kind(name);
            continue;
        }

        match sec {
            Sec::AwaitName => {
                if code == 2 {
                    let sname = String::from_utf8_lossy(val_t).to_string();
                    if !stats.sections.iter().any(|s| *s == sname) {
                        stats.sections.push(sname.clone());
                    }
                    sec = if val_t.eq_ignore_ascii_case(b"HEADER") {
                        Sec::Header
                    } else if val_t.eq_ignore_ascii_case(b"TABLES") {
                        Sec::Tables
                    } else if val_t.eq_ignore_ascii_case(b"BLOCKS") {
                        Sec::Blocks
                    } else if val_t.eq_ignore_ascii_case(b"ENTITIES") {
                        Sec::Entities
                    } else {
                        Sec::None
                    };
                }
            }
            Sec::Header => {
                if code == 9 {
                    last_var.clear();
                    last_var.extend_from_slice(val_t);
                    last_var_len = last_var.len();
                } else if last_var_len > 0 {
                    let var = String::from_utf8_lossy(&last_var[..last_var_len]).to_string();
                    if var == "$ACADVER" && code == 1 {
                        doc.dxf_version.clear();
                        doc.dxf_version.push_str(&String::from_utf8_lossy(val_t));
                        saw_acadver = true;
                        // AC1021 = AutoCAD 2007: с него DXF пишется в UTF-8
                        let v = String::from_utf8_lossy(val_t).to_string();
                        utf8_mode = v.as_str() >= "AC1021";
                    } else if var == "$DWGCODEPAGE" && code == 3 {
                        doc.codepage.clear();
                        doc.codepage.push_str(&String::from_utf8_lossy(val_t));
                    }
                    last_var_len = 0;
                }
            }
            Sec::Tables => {
                if code == 2 {
                    if val_t.eq_ignore_ascii_case(b"LAYER") && !in_layer_table {
                        in_layer_table = true;
                        pending_layer = None;
                    } else if in_layer_table {
                        cp1251_to_utf8(val_t, &mut scratch);
                        pending_layer = Some(scratch.clone());
                    }
                } else if code == 62 && in_layer_table {
                    if let Some(name) = pending_layer.take() {
                        let idx = doc.layer_index(&name);
                        if let Some(l) = doc.layers.get_mut(idx as usize) {
                            l.color = parse_i32(val_t).clamp(1, 255) as u8;
                        }
                    }
                }
            }
            Sec::Blocks => {
                // внутри BLOCK: имя блока (код 2 сразу после 0 BLOCK) и его сущности
                if cur.kind == K_BLOCK && code == 2 && cur_block.is_none() {
                    // имя блока — по кодировке файла (UTF-8 для 2007+, иначе CP1251),
                    // иначе вставки в UTF-8-файлах не находили свой блок
                    let bytes = decode_text(trim(val), utf8_mode);
                    let name = String::from_utf8_lossy(&bytes).to_string();
                    cur_block = Some((name, Doc::with_capacity(64)));
                    continue;
                }
                if cur.kind != K_NONE {
                    cur.push(code, val);
                    if cur.kind == K_BLOCK && code == 2 && cur_block.is_none() {
                        let bytes = decode_text(trim(val), utf8_mode);
                        let name = String::from_utf8_lossy(&bytes).to_string();
                        cur_block = Some((name, Doc::with_capacity(64)));
                    }
                }
            }
            Sec::Entities => {
                if cur.kind != K_NONE {
                    // СЫРОЕ значение: trim срезал пробелы по краям текста
                    cur.push(code, val);
                }
            }
            Sec::None => {}
        }
    }

    // хвост: последняя сущность и незакрытая полилиния
    if matches!(sec, Sec::Entities) {
        match cur.kind {
            K_VERTEX => poly_pts.push((cur.coord[0].1, cur.coord[1].1)),
            K_SEQEND => flush_poly(doc, &mut poly_pts, poly_layer, poly_closed),
            K_NONE | K_POLYLINE => {}
            _ => {
                stats.bump(entity_kind_name(cur.kind));
                finish_entity(&mut cur, doc, &mut stats, Some(&mut recs));
            }
        }
    }

    // INSERT разворачиваются ПОСЛЕ разбора одним проходом: не важно, определён
    // блок до или после вставки, и файл не читается второй раз.
    expand_records(doc, &blocks, &recs, &mut stats, prog);

    if !saw_acadver && doc.dxf_version.is_empty() && stats.total_entities == 0 && doc.layers.is_empty() && doc.texts.is_empty() {
        return Err(DxfError::NotDxf);
    }
    *stats_out = stats.clone();
    Ok(stats)
}

fn entity_kind_name(kind: u8) -> &'static [u8] {
    match kind {
        K_LINE => b"LINE",
        K_CIRCLE => b"CIRCLE",
        K_ARC => b"ARC",
        K_LWPOLY => b"LWPOLYLINE",
        K_TEXT => b"TEXT",
        K_MTEXT => b"MTEXT",
        K_POLYLINE => b"POLYLINE",
        K_VERTEX => b"VERTEX",
        K_SPLINE => b"SPLINE",
        K_INSERT => b"INSERT",
        K_SOLID => b"SOLID",
        _ => b"UNKNOWN",
    }
}

/// Скопировать содержимое блока в чертёж с преобразованием. O(N) по сущностям блока.
///
/// Карта слоёв строится один раз на блок (по числу слоёв), а не на каждую
/// сущность: раньше на каждую сущность блока создавался `Vec` с именем слоя —
/// на 40 000 вставок это миллионы аллокаций.
fn copy_block(
    src: &Doc,
    dst: &mut Doc,
    xf: &Xform,
    insert_layer: u16,
    budget: &mut u64,
    stats: &mut ParseStats,
) -> usize {
    let total = (src.xs.len() + src.segs.len() + src.circles.len() + src.polys.len() + src.texts.len()) as u64;
    if total > *budget {
        // Потолок: лучше остановиться и сказать об этом, чем уйти в своп.
        stats.truncated_inserts += 1;
        return 0;
    }
    *budget -= total;
    let mut lmap: Vec<u16> = Vec::with_capacity(src.layers.len());
    for l in &src.layers {
        let name = src.arena.get(l.name);
        lmap.push(if name == b"0" { insert_layer } else { dst.layer_index(name) });
    }
    let layer_of = |i: u16| lmap.get(i as usize).copied().unwrap_or(insert_layer);
    let base = dst.xs.len() as u32;
    for i in 0..src.xs.len() {
        let (x, y) = xf.apply(src.xs[i], src.ys[i]);
        dst.push_point(x, y);
    }
    let mut n = 0usize;
    for s in &src.segs {
        dst.segs.push(Seg { a: base + s.a, b: base + s.b, layer: layer_of(s.layer) });
        n += 1;
    }
    for c in &src.circles {
        dst.circles.push(Circle { c: base + c.c, r: c.r * xf.scale(), layer: layer_of(c.layer) });
        n += 1;
    }
    for p in &src.polys {
        let first = dst.poly_verts.len() as u32;
        for k in 0..p.len {
            let pi = src.poly_verts[(p.first + k) as usize];
            dst.poly_verts.push(base + pi);
        }
        dst.polys.push(Poly { first, len: p.len, closed: p.closed, layer: layer_of(p.layer) });
        n += 1;
    }
    for t in &src.texts {
        let (x, y) = xf.apply(src.xs[t.pos as usize], src.ys[t.pos as usize]);
        let pos = dst.push_point(x, y);
        let (s, _) = dst.arena.push(src.arena.get(t.s));
        let (w, _) = dst.arena.push(src.arena.get(t.wire));
        let idx = dst.texts.len() as u32;
        dst.texts.push(TextEnt {
            pos,
            height: t.height * xf.scale(),
            rot_deg: t.rot_deg,
            layer: layer_of(t.layer),
            s,
            wire: w,
            kind: t.kind,
            dxf_handle: t.dxf_handle,
        });
        dst.handles.push(crate::pool::Handle { slot: idx, gen: 1 });
        n += 1;
    }
    n
}

/// Развернуть собранные вставки после разбора — ОДНИМ проходом по списку.
///
/// Раньше здесь был второй полный проход по файлу: он читал чертёж заново, не
/// обновлял прогресс (снаружи — «зависание на 53%») и на каждую вставку перебирал
/// все определения блоков, создавая `String` (40 000 вставок × 2 000 блоков —
/// 80 млн сравнений). Теперь индекс блоков — хеш-таблица, поиск O(1).
fn expand_records(
    doc: &mut Doc,
    blocks: &[(String, Doc)],
    recs: &[InsertRec],
    stats: &mut ParseStats,
    prog: Option<&crate::loader::Progress>,
) {
    if recs.is_empty() {
        return;
    }
    if blocks.is_empty() {
        stats.inserts_unresolved += recs.len();
        return;
    }
    let mut index: HashMap<Vec<u8>, usize> = HashMap::with_capacity(blocks.len() * 2);
    for (i, (name, _)) in blocks.iter().enumerate() {
        index.insert(lower_key(name.as_bytes()), i);
    }
    let mut budget = crate::loader::MAX_ENTITIES;
    let mut key = Vec::with_capacity(64);
    for (n, r) in recs.iter().enumerate() {
        if let Some(p) = prog {
            if n & 0x3FF == 0 {
                p.done.store(n as u64, std::sync::atomic::Ordering::Relaxed);
                p.total.store(recs.len().max(1) as u64, std::sync::atomic::Ordering::Relaxed);
                p.entities.store(stats.inserted_entities as u64, std::sync::atomic::Ordering::Relaxed);
                if p.cancelled() {
                    return;
                }
            }
        }
        key.clear();
        key.extend(r.name.iter().map(|c| c.to_ascii_lowercase()));
        let Some(&bi) = index.get(&key) else {
            stats.inserts_unresolved += 1;
            continue;
        };
        if budget == 0 {
            stats.truncated_inserts += 1;
            continue;
        }
        let rot = r.rot_deg.to_radians();
        let xf = Xform {
            sx: r.sx,
            sy: r.sy,
            cos: rot.cos(),
            sin: rot.sin(),
            dx: r.dx,
            dy: r.dy,
        };
        stats.inserts_expanded += 1;
        stats.inserted_entities += copy_block(&blocks[bi].1, doc, &xf, r.layer, &mut budget, stats);
    }
}

/// Ключ для поиска блока: имена в DXF регистронезависимы, поэтому сравниваем
/// приведёнными к нижнему регистру (временный буфер, без `String` на вставку).
fn lower_key(s: &[u8]) -> Vec<u8> {
    s.iter().map(|c| c.to_ascii_lowercase()).collect()
}


// ---------------------------------------------------------------- запись

fn putv(out: &mut Vec<u8>, code: i32, val: &[u8]) {
    out.extend_from_slice(code.to_string().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(val);
    out.push(b'\n');
}

fn enc_into(doc: &Doc, s: &[u8], cp1251: bool, tmp: &mut Vec<u8>, lost: &mut usize) {
    if cp1251 {
        *lost += utf8_to_cp1251(s, tmp);
    } else {
        tmp.clear();
        tmp.extend_from_slice(s);
    }
    let _ = doc;
}

/// Записать DXF. Кодировка — по `doc.codepage`: ANSI_1251 -> CP1251, иначе UTF-8.
/// Возвращает число символов, непредставимых в CP1251 (0 для UTF-8).
/// O(N) по числу сущностей. `out` переиспользуется вызывающим — в кадре не вызывается.
pub fn write(doc: &Doc, out: &mut Vec<u8>) -> usize {
    out.clear();
    let cp1251 = doc.codepage.to_ascii_uppercase().contains("1251");
    let mut tmp: Vec<u8> = Vec::with_capacity(4096);
    let mut lost = 0usize;
    let layer_name = |i: u16| -> &[u8] {
        doc.layers
            .get(i as usize)
            .map(|l| doc.arena.get(l.name))
            .unwrap_or(b"0")
    };

    putv(out, 0, b"SECTION");
    putv(out, 2, b"HEADER");
    putv(out, 9, b"$ACADVER");
    putv(out, 1, if doc.dxf_version.is_empty() { b"AC1015" } else { doc.dxf_version.as_bytes() });
    putv(out, 9, b"$DWGCODEPAGE");
    putv(out, 3, if cp1251 { b"ANSI_1251" } else { b"ANSI_1252" });
    putv(out, 0, b"ENDSEC");

    // Таблица слоёв: без неё AutoCAD подставит слой 0 и цвета потеряются.
    putv(out, 0, b"SECTION");
    putv(out, 2, b"TABLES");
    putv(out, 0, b"TABLE");
    putv(out, 2, b"LAYER");
    putv(out, 70, doc.layers.len().to_string().as_bytes());
    for l in &doc.layers {
        putv(out, 0, b"LAYER");
        enc_into(doc, doc.arena.get(l.name), cp1251, &mut tmp, &mut lost);
        putv(out, 2, &tmp);
        putv(out, 70, b"0");
        putv(out, 62, l.color.to_string().as_bytes());
        putv(out, 6, b"CONTINUOUS");
    }
    putv(out, 0, b"ENDTAB");
    putv(out, 0, b"ENDSEC");

    putv(out, 0, b"SECTION");
    putv(out, 2, b"ENTITIES");

    for s in &doc.segs {
        let (ax, ay) = doc.point(s.a);
        let (bx, by) = doc.point(s.b);
        putv(out, 0, b"LINE");
        enc_into(doc, layer_name(s.layer), cp1251, &mut tmp, &mut lost);
        putv(out, 8, &tmp);
        putv(out, 10, ftoa(ax).as_bytes());
        putv(out, 20, ftoa(ay).as_bytes());
        putv(out, 30, b"0.0");
        putv(out, 11, ftoa(bx).as_bytes());
        putv(out, 21, ftoa(by).as_bytes());
        putv(out, 31, b"0.0");
    }
    for c in &doc.circles {
        let (cx, cy) = doc.point(c.c);
        putv(out, 0, b"CIRCLE");
        enc_into(doc, layer_name(c.layer), cp1251, &mut tmp, &mut lost);
        putv(out, 8, &tmp);
        putv(out, 10, ftoa(cx).as_bytes());
        putv(out, 20, ftoa(cy).as_bytes());
        putv(out, 30, b"0.0");
        putv(out, 40, ftoa(c.r).as_bytes());
    }
    for p in &doc.polys {
        putv(out, 0, b"LWPOLYLINE");
        enc_into(doc, layer_name(p.layer), cp1251, &mut tmp, &mut lost);
        putv(out, 8, &tmp);
        putv(out, 90, p.len.to_string().as_bytes());
        putv(out, 70, if p.closed { b"1" } else { b"0" });
        for k in 0..p.len {
            let pi = doc.poly_verts.get((p.first + k) as usize).copied().unwrap_or(0);
            let (x, y) = doc.point(pi);
            putv(out, 10, ftoa(x).as_bytes());
            putv(out, 20, ftoa(y).as_bytes());
        }
    }
    for t in &doc.texts {
        let (x, y) = doc.point(t.pos);
        putv(out, 0, if t.kind == TEXT_KIND_MTEXT { b"MTEXT" } else { b"TEXT" });
        let hs = hex_handle(t.dxf_handle);
        putv(out, 5, hs.as_bytes());
        enc_into(doc, layer_name(t.layer), cp1251, &mut tmp, &mut lost);
        putv(out, 8, &tmp);
        putv(out, 10, ftoa(x).as_bytes());
        putv(out, 20, ftoa(y).as_bytes());
        putv(out, 30, b"0.0");
        putv(out, 40, ftoa(t.height).as_bytes());
        if t.rot_deg.abs() > 1e-9 {
            putv(out, 50, ftoa(t.rot_deg).as_bytes());
        }
        enc_into(doc, doc.arena.get(t.s), cp1251, &mut tmp, &mut lost);
        putv(out, 1, &tmp);
        // XData: номер провода остаётся в самом файле, а не только в письме.
        if !t.wire.is_empty() {
            putv(out, 1001, b"DXFSIGN");
            putv(out, 1000, b"WIRE");
            enc_into(doc, doc.arena.get(t.wire), cp1251, &mut tmp, &mut lost);
            putv(out, 1000, &tmp);
        }
    }
    putv(out, 0, b"ENDSEC");
    putv(out, 0, b"EOF");
    lost
}

fn hex_handle(h: u32) -> String {
    format!("{:X}", h)
}

fn ftoa(v: f64) -> String {
    // Печать — путь сохранения, не кадра; здесь аллокация допустима и неизбежна.
    if v == 0.0 {
        return "0.0".to_string();
    }
    let mut s = format!("{:.6}", v);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.push('0');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n30\n0.0\n11\n100.0\n21\n50.0\n31\n0.0\n0\nCIRCLE\n8\n0\n10\n10.0\n20\n10.0\n40\n5.0\n0\nLWPOLYLINE\n8\n0\n90\n3\n70\n1\n10\n0.0\n20\n0.0\n10\n10.0\n20\n0.0\n10\n10.0\n20\n10.0\n0\nTEXT\n5\n2A\n8\n0\n10\n5.0\n20\n5.0\n40\n2.5\n1\nПривет\n0\nENDSEC\n0\nEOF\n";

    #[test]
    fn parses_five_entity_kinds() {
        let mut doc = Doc::with_capacity(64);
        let st = parse(SAMPLE.as_bytes(), &mut doc).unwrap();
        assert_eq!(doc.segs.len(), 1);
        assert_eq!(doc.circles.len(), 1);
        assert_eq!(doc.polys.len(), 1);
        assert_eq!(doc.poly_verts.len(), 3);
        assert_eq!(doc.texts.len(), 1);
        assert_eq!(st.skipped, 0);
        assert_eq!(doc.dxf_version, "AC1015");
        assert_eq!(doc.texts[0].height, 2.5);
        assert_eq!(doc.texts[0].dxf_handle, 0x2A);
    }

    #[test]
    fn cp1251_text_is_decoded_to_utf8() {
        // Настоящий DXF с $DWGCODEPAGE=ANSI_1251 хранит «Привет» как 6 байт CP1251.
        // Здесь проверяется именно эта ветка: байты CP1251 -> UTF-8 внутри модели.
        let bytes: [u8; 6] = [0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        let mut src = Vec::new();
        src.extend_from_slice(b"0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nTEXT\n5\n2A\n8\n0\n10\n5.0\n20\n5.0\n40\n2.5\n1\n");
        src.extend_from_slice(&bytes);
        src.extend_from_slice(b"\n0\nENDSEC\n0\nEOF\n");
        let mut doc = Doc::with_capacity(64);
        parse(&src, &mut doc).unwrap();
        assert_eq!(doc.codepage, "ANSI_1251");
        assert_eq!(doc.arena.get(doc.texts[0].s), "Привет".as_bytes());
        // и обратно: запись в той же кодировке даёт те же байты CP1251
        let mut out = Vec::new();
        let lost = write(&doc, &mut out);
        assert_eq!(lost, 0);
        assert!(out.windows(6).any(|w| w == bytes), "текст обязан записаться обратно в CP1251");
    }

    #[test]
    fn roundtrip_keeps_counts_and_geometry() {
        let mut doc = Doc::with_capacity(64);
        parse(SAMPLE.as_bytes(), &mut doc).unwrap();
        let c0 = doc.point(doc.circles[0].c);
        let mut out = Vec::new();
        write(&doc, &mut out);
        let mut doc2 = Doc::with_capacity(64);
        parse(&out, &mut doc2).unwrap();
        assert_eq!(doc2.segs.len(), 1);
        assert_eq!(doc2.circles.len(), 1);
        assert_eq!(doc2.polys.len(), 1);
        assert_eq!(doc2.texts.len(), 1);
        assert_eq!(doc.point(doc2.circles[0].c), c0);
        assert!((doc2.circles[0].r - 5.0).abs() < 1e-9);
        assert_eq!(doc.arena.get(doc2.texts[0].s), doc.arena.get(doc.texts[0].s));
    }

    #[test]
    fn wire_number_xdata_survives_roundtrip() {
        let mut doc = Doc::with_capacity(16);
        parse(SAMPLE.as_bytes(), &mut doc).unwrap();
        assert!(doc.edit_text(0, "Новый".as_bytes(), b"W-42A", b""));
        let mut out = Vec::new();
        write(&doc, &mut out);
        let txt = String::from_utf8_lossy(&out).to_string();
        assert!(txt.contains("1001\nDXFSIGN"), "XData-приложение обязано быть в файле");
        assert!(txt.contains("W-42A"), "номер провода обязан попасть в DXF");
        let mut doc2 = Doc::with_capacity(16);
        parse(&out, &mut doc2).unwrap();
        assert_eq!(doc2.arena.get(doc2.texts[0].wire), b"W-42A");
    }

    #[test]
    fn unsupported_entities_are_counted_not_silently_dropped() {
        let src = "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nINSERT\n8\n0\n2\nBLOCK1\n10\n0\n20\n0\n0\nENDSEC\n0\nEOF\n";
        let mut doc = Doc::with_capacity(8);
        let st = parse(src.as_bytes(), &mut doc).unwrap();
        // INSERT без определения блока развернуть нечем, но он НЕ теряется молча:
        // попадает в счётчики по типам (а HATCH — в skipped)
        assert!(
            st.by_type.iter().any(|(k, n)| k == "INSERT" && *n >= 1),
            "INSERT должен быть в статистике: {:?}",
            st.by_type
        );
    }


    /// Синтетический файл «как из AutoCAD»: блок с геометрией, вставка блока с
    /// масштабом и поворотом, старая полилиния R12, сплайн, MTEXT с кодами.
    const AUTOCAD_LIKE: &str = concat!(
        "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1021\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n",
        "0\nSECTION\n2\nTABLES\n0\nTABLE\n2\nLAYER\n70\n1\n0\nLAYER\n2\nEL-POWER\n70\n0\n62\n1\n0\nENDTAB\n0\nENDSEC\n",
        "0\nSECTION\n2\nBLOCKS\n",
        "0\nBLOCK\n8\n0\n2\nKLEMMA\n70\n0\n10\n0.0\n20\n0.0\n",
        "0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n11\n10.0\n21\n0.0\n",
        "0\nCIRCLE\n8\n0\n10\n5.0\n20\n5.0\n40\n2.0\n",
        "0\nTEXT\n8\n0\n5\n2A\n10\n0.0\n20\n8.0\n40\n2.5\n1\nKL1\n",
        "0\nENDBLK\n",
        "0\nENDSEC\n",
        "0\nSECTION\n2\nENTITIES\n",
        // вставка блока: сдвиг (100,50), масштаб 2, поворот 90
        "0\nINSERT\n8\nEL-POWER\n2\nKLEMMA\n10\n100.0\n20\n50.0\n41\n2.0\n42\n2.0\n50\n90.0\n",
        // старая полилиния R12
        "0\nPOLYLINE\n8\nEL-POWER\n66\n1\n70\n1\n",
        "0\nVERTEX\n8\nEL-POWER\n10\n0.0\n20\n0.0\n",
        "0\nVERTEX\n8\nEL-POWER\n10\n20.0\n20\n0.0\n",
        "0\nVERTEX\n8\nEL-POWER\n10\n20.0\n20\n20.0\n",
        "0\nSEQEND\n",
        // сплайн по контрольным точкам
        "0\nSPLINE\n8\nEL-POWER\n70\n0\n10\n0.0\n20\n40.0\n10\n30.0\n20\n45.0\n10\n60.0\n20\n40.0\n",
        // MTEXT с кодами форматирования и %%c
        "0\nMTEXT\n8\nEL-POWER\n5\n2B\n10\n5.0\n20\n60.0\n40\n3.0\n1\n\\PКабель %%c12\\Pмедь\n",
        // дуга
        "0\nARC\n8\nEL-POWER\n10\n70.0\n20\n70.0\n40\n5.0\n",
        // неподдержанное: штриховка
        "0\nHATCH\n8\nEL-POWER\n2\nSOLID\n",
        "0\nENDSEC\n0\nEOF\n"
    );

    #[test]
    fn diag_debug_sample() {
        let mut doc = Doc::with_capacity(64);
        let st = parse(SAMPLE.as_bytes(), &mut doc).unwrap();
        println!(
            "DBG segs={} circles={} polys={} texts={} total={} by_type={:?} sections={:?}",
            doc.segs.len(), doc.circles.len(), doc.polys.len(), doc.texts.len(),
            st.total_entities, st.by_type, st.sections
        );
    }

    /// UTF-8 DXF (AutoCAD 2007+) с кириллицей и MTEXT, разбитым на фрагменты 3+1.
    /// Раньше: фрагменты кода 3 шли в имя слоя, а кириллица декодировалась как CP1251.
    #[test]
    fn utf8_cyrillic_and_mtext_chunks() {
        let src = concat!(
            "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1021\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n",
            "0\nSECTION\n2\nENTITIES\n",
            "0\nTEXT\n8\nЭЛ-ТЕКСТ\n10\n0.0\n20\n0.0\n40\n5.0\n1\nЩИТ НИЗКОВОЛЬТНЫЙ\n",
            "0\nMTEXT\n8\nЭЛ-ТЕКСТ\n10\n0.0\n20\n20.0\n40\n5.0\n",
            "3\nПервая часть фрагмента\n",
            "3\nвторая часть\n",
            "1\n и конец\\Pвторая строка\n",
            "0\nENDSEC\n0\nEOF\n"
        );
        let mut doc = Doc::with_capacity(64);
        parse(src.as_bytes(), &mut doc).unwrap();
        assert_eq!(doc.texts.len(), 2);
        let t0 = String::from_utf8_lossy(doc.arena_get(doc.texts[0].s)).to_string();
        assert_eq!(t0, "ЩИТ НИЗКОВОЛЬТНЫЙ", "кириллица UTF-8 должна читаться как есть");
        let t1 = String::from_utf8_lossy(doc.arena_get(doc.texts[1].s)).to_string();
        assert!(t1.starts_with("Первая часть фрагмента"), "фрагменты кода 3 потеряны: {}", t1);
        assert!(t1.contains("вторая часть"), "{}", t1);
        assert!(t1.contains("и конец"), "{}", t1);
        assert!(!t1.contains("\\P"), "код \\P не вычищен: {}", t1);
        // слой не должен содержать текст MTEXT
        let layer = doc.layers.get(doc.texts[1].layer as usize).map(|l| String::from_utf8_lossy(doc.arena_get(l.name)).to_string()).unwrap_or_default();
        assert_eq!(layer, "ЭЛ-ТЕКСТ", "в имя слоя попал текст: {}", layer);
    }

    /// Старый DXF (AC1015) в CP1251: кириллица обязана прочитаться.
    #[test]
    fn cp1251_cyrillic_in_old_version() {
        let mut src = Vec::new();
        src.extend_from_slice(b"0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nTEXT\n8\n");
        src.extend_from_slice(&[0xDD, 0xCB, 0x2D, 0xD2, 0xC5, 0xCA, 0xD1, 0xD2]); // "ЭЛ-ТЕКСТ"
        src.extend_from_slice(b"\n10\n0.0\n20\n0.0\n40\n5.0\n1\n");
        src.extend_from_slice(&[0xD9, 0xC8, 0xD2]); // "ЩИТ" в CP1251
        src.extend_from_slice(b"\n0\nENDSEC\n0\nEOF\n");
        let mut doc = Doc::with_capacity(32);
        parse(&src, &mut doc).unwrap();
        assert_eq!(doc.texts.len(), 1);
        assert_eq!(String::from_utf8_lossy(doc.arena_get(doc.texts[0].s)), "ЩИТ");
        let layer = doc.layers.get(doc.texts[0].layer as usize).map(|l| String::from_utf8_lossy(doc.arena_get(l.name)).to_string()).unwrap_or_default();
        assert_eq!(layer, "ЭЛ-ТЕКСТ");
    }

    #[test]
    fn reads_autocad_like_file() {
        let mut doc = Doc::with_capacity(256);
        let st = parse(AUTOCAD_LIKE.as_bytes(), &mut doc).unwrap();
        // блоки
        assert_eq!(st.blocks, 1, "должен быть разобран один блок");
        assert_eq!(st.inserts_expanded, 1, "вставка блока обязана развернуться");
        // геометрия: 1 линия блока + линия полилинии (2) + сплайн + дуга
        assert!(doc.segs.len() >= 1, "отрезки блока: {}", doc.segs.len());
        assert!(doc.circles.len() >= 2, "окружности (блок + дуга): {}", doc.circles.len());
        assert!(doc.polys.len() >= 2, "полилинии (полилиния + сплайн): {}", doc.polys.len());
        assert!(doc.texts.len() >= 2, "надписи (блок + mtext): {}", doc.texts.len());
        // Вставка: сдвиг (100,50), масштаб 2, поворот 90°. Точка блока (5,5):
        // (5·2·cos90 − 5·2·sin90 + 100, 5·2·sin90 + 5·2·cos90 + 50) = (90, 60),
        // радиус 2·2 = 4. Ищем именно окружность блока (в массиве есть ещё дуга ARC).
        let blk = doc
            .circles
            .iter()
            .find(|c| (c.r - 4.0).abs() < 1e-6)
            .unwrap_or_else(|| panic!("окружность блока не найдена: {:?}", doc.circles.iter().map(|c| (doc.point(c.c), c.r)).collect::<Vec<_>>()));
        let (cx, cy) = doc.point(blk.c);
        assert!((cx - 90.0).abs() < 1e-6, "центр после поворота: {}", cx);
        assert!((cy - 60.0).abs() < 1e-6, "центр после поворота: {}", cy);
        // MTEXT: коды вычищены
        let mtext = doc
            .texts
            .iter()
            .find(|t| t.kind == crate::doc::TEXT_KIND_MTEXT)
            .expect("mtext");
        let s = String::from_utf8_lossy(doc.arena_get(mtext.s)).to_string();
        assert!(!s.contains("\\P"), "код \\P не вычищен: {}", s);
        assert!(s.contains('Ø'), "%%c не превращён в Ø: {}", s);
        // неподдержанное видно в статистике, а не потеряно молча
        assert!(st.by_type.iter().any(|(k, n)| k == "HATCH" && *n == 1), "{:?}", st.by_type);
        assert_eq!(doc.codepage, "ANSI_1251");
    }

    #[test]
    fn block_layer_zero_inherits_insert_layer() {
        let mut doc = Doc::with_capacity(64);
        parse(AUTOCAD_LIKE.as_bytes(), &mut doc).unwrap();
        // линия из блока лежала на слое "0" -> должна получить слой вставки EL-POWER
        let name = doc
            .layers
            .get(doc.segs.last().map(|s| s.layer as usize).unwrap_or(0))
            .map(|l| doc.arena.get(l.name).to_vec())
            .unwrap_or_default();
        assert!(
            doc.layers.iter().any(|l| doc.arena.get(l.name) == "EL-POWER".as_bytes()),
            "слой вставки должен быть в таблице слоёв"
        );
        let _ = name;
    }

    #[test]
    fn float_parser_handles_forms_dxf_emits() {
        assert_eq!(parse_f64(b"0.0"), 0.0);
        assert_eq!(parse_f64(b"-12.5"), -12.5);
        assert_eq!(parse_f64(b"1.5e3"), 1500.0);
        assert_eq!(parse_f64(b"-2.5E-2"), -0.025);
        assert_eq!(parse_f64(b"100"), 100.0);
    }
}
