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

use crate::doc::{Circle, Doc, Poly, Seg, TextEnt, TEXT_KIND_MTEXT, TEXT_KIND_TEXT};
use crate::pool::Handle;
use crate::text::{cp1251_to_utf8, utf8_to_cp1251};

#[derive(Debug)]
pub enum DxfError {
    NotDxf,
    Eof,
    BadCode { line: usize },
    Unsupported(String),
    Io(String),
}

impl std::fmt::Display for DxfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DxfError::NotDxf => write!(f, "файл не похож на ASCII DXF"),
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
}

impl<'a> TagReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, line: 0 }
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
    fn next_tag(&mut self) -> Option<(i32, &'a [u8])> {
        let code_line = self.read_line()?;
        let code = parse_code(code_line)?;
        let value = self.read_line()?;
        Some((code, value))
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
#[derive(Debug, Default, Clone)]
pub struct ParseStats {
    pub total_entities: usize,
    pub skipped: usize,
    pub skipped_kinds: [u32; 8],
}

/// Разобрать ASCII DXF в `doc`. `doc` переиспользуется (capacity сохраняется),
/// поэтому повторное открытие файла того же размера не аллоцирует.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sec {
    None,
    AwaitName,
    Header,
    Tables,
    Entities,
}

/// Разобрать ASCII DXF в `doc` одним проходом. `doc` переиспользуется
/// (capacity сохраняется), поэтому повторное открытие файла того же размера
/// не аллоцирует на типовых чертежах.
///
/// Почему один проход, а не вложенные читатели: первая версия звала
/// `read_header`/`read_tables`, и они съедали пару `0 SECTION / 2 ENTITIES`,
/// после чего геометрия молча не читалась. Состояние секции теперь одно и
/// живёт в самом цикле — такую ошибку больше негде спрятать.
pub fn parse(data: &[u8], doc: &mut Doc) -> Result<ParseStats> {
    doc.clear();
    let mut stats = ParseStats::default();
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

    loop {
        let (code, val) = match r.next_tag() {
            Some(t) => t,
            None => break,
        };
        let val_t = trim(val);
        if code == 0 {
            let name = val_t;
            if name.eq_ignore_ascii_case(b"SECTION") {
                if sec == Sec::Entities && cur.kind != K_NONE {
                    finish_entity(&mut cur, doc, &mut stats);
                    cur.reset();
                }
                sec = Sec::AwaitName;
                continue;
            }
            if name.eq_ignore_ascii_case(b"ENDSEC") {
                if sec == Sec::Entities && cur.kind != K_NONE {
                    finish_entity(&mut cur, doc, &mut stats);
                    cur.reset();
                }
                sec = Sec::None;
                in_layer_table = false;
                continue;
            }
            if name.eq_ignore_ascii_case(b"EOF") {
                break;
            }
            match sec {
                Sec::Entities => {
                    if cur.kind != K_NONE {
                        finish_entity(&mut cur, doc, &mut stats);
                    }
                    cur.reset();
                    cur.set_kind(name);
                }
                Sec::Tables => {
                    if name.eq_ignore_ascii_case(b"TABLE") {
                        in_layer_table = false;
                    } else if name.eq_ignore_ascii_case(b"ENDTAB") {
                        in_layer_table = false;
                    } else if name.eq_ignore_ascii_case(b"LAYER") {
                        pending_layer = None;
                    }
                }
                _ => {}
            }
            continue;
        }

        match sec {
            Sec::AwaitName => {
                if code == 2 {
                    sec = if val_t.eq_ignore_ascii_case(b"HEADER") {
                        Sec::Header
                    } else if val_t.eq_ignore_ascii_case(b"TABLES") {
                        Sec::Tables
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
            Sec::Entities => {
                if cur.kind != K_NONE {
                    cur.push(code, val_t);
                }
            }
            Sec::None => {}
        }
    }
    if sec == Sec::Entities && cur.kind != K_NONE {
        finish_entity(&mut cur, doc, &mut stats);
    }
    if !saw_acadver && doc.dxf_version.is_empty() && stats.total_entities == 0 && doc.layers.is_empty() {
        return Err(DxfError::NotDxf);
    }
    Ok(stats)
}

#[derive(Default)]
struct EntityBuf {
    kind: u8,
    layer: u16,
    coord: [(bool, f64); 6], // 10,20,30,11,21,31
    radius: f64,
    height: f64,
    rot_deg: f64,
    flags: i32,
    count: i32,
    handle: u32,
    s1: Vec<u8>,
    s3: Vec<u8>,
    app: Vec<u8>,
    wire: Vec<u8>,
    want_wire: bool,
    verts: Vec<(f64, f64)>,
    pending_x: Option<f64>,
}

const K_NONE: u8 = 0;
const K_LINE: u8 = 1;
const K_CIRCLE: u8 = 2;
const K_LWPOLY: u8 = 3;
const K_TEXT: u8 = 4;
const K_MTEXT: u8 = 5;
const K_UNKNOWN: u8 = 6;

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
        self.s3.clear();
        self.app.clear();
        self.wire.clear();
        self.want_wire = false;
        self.verts.clear();
        self.pending_x = None;
    }

    fn set_kind(&mut self, name: &[u8]) {
        self.kind = if name.eq_ignore_ascii_case(b"LINE") {
            K_LINE
        } else if name.eq_ignore_ascii_case(b"CIRCLE") {
            K_CIRCLE
        } else if name.eq_ignore_ascii_case(b"LWPOLYLINE") {
            K_LWPOLY
        } else if name.eq_ignore_ascii_case(b"TEXT") {
            K_TEXT
        } else if name.eq_ignore_ascii_case(b"MTEXT") {
            K_MTEXT
        } else {
            K_UNKNOWN
        };
    }

    fn push(&mut self, code: i32, val: &[u8]) {
        match code {
            8 => {
                let mut b = Vec::new();
                cp1251_to_utf8(trim(val), &mut b);
                self.s3 = b;
            }
            5 => self.handle = parse_hex_u32(val),
            1 => {
                cp1251_to_utf8(trim(val), &mut self.s1);
            }
            3 => {
                // MTEXT: дополнительные фрагменты идут в 3 ДО основного 1
                let mut b = Vec::new();
                cp1251_to_utf8(trim(val), &mut b);
                self.s3.extend_from_slice(&b);
            }
            10 => self.set_coord(0, parse_f64(val)),
            20 => self.set_coord(1, parse_f64(val)),
            30 => self.set_coord(2, parse_f64(val)),
            11 => self.set_coord(3, parse_f64(val)),
            21 => self.set_coord(4, parse_f64(val)),
            31 => self.set_coord(5, parse_f64(val)),
            40 => self.height = parse_f64(val),
            42 => self.radius = parse_f64(val),
            50 => self.rot_deg = parse_f64(val),
            70 => self.flags = parse_i32(val),
            90 => self.count = parse_i32(val),
            1001 => {
                self.app.clear();
                self.app.extend_from_slice(trim(val));
                self.want_wire = false;
            }
            1000 => {
                let mut b = Vec::new();
                cp1251_to_utf8(trim(val), &mut b);
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
        if self.kind == K_LWPOLY {
            // LWPOLYLINE: 10 идёт без 20 между ними только для 2D-вершин в порядке пар
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

fn finish_entity(e: &mut EntityBuf, doc: &mut Doc, stats: &mut ParseStats) {
    stats.total_entities += 1;
    match e.kind {
        K_LINE => {
            let (ax, ay) = (e.coord[0].1, e.coord[1].1);
            let (bx, by) = (e.coord[3].1, e.coord[4].1);
            let a = doc.push_point(ax, ay);
            let b = doc.push_point(bx, by);
            let layer = doc.layer_index(&e.s3);
            doc.segs.push(Seg { a, b, layer });
        }
        K_CIRCLE => {
            let c = doc.push_point(e.coord[0].1, e.coord[1].1);
            let layer = doc.layer_index(&e.s3);
            doc.circles.push(Circle { c, r: e.height.abs(), layer });
        }
        K_LWPOLY => {
            if e.verts.len() >= 2 {
                let first = doc.poly_verts.len() as u32;
                for &(x, y) in &e.verts {
                    let i = doc.push_point(x, y);
                    doc.poly_verts.push(i);
                }
                let layer = doc.layer_index(&e.s3);
                doc.polys.push(Poly {
                    first,
                    len: e.verts.len() as u32,
                    closed: (e.flags & 1) != 0,
                    layer,
                });
            }
        }
        K_TEXT | K_MTEXT => {
            let pos = doc.push_point(e.coord[0].1, e.coord[1].1);
            let layer = doc.layer_index(&e.s3);
            let (s, _) = doc.arena.push(&e.s1);
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
            doc.handles.push(Handle { slot: idx as u32, gen: 1 });
        }
        K_UNKNOWN => {
            stats.skipped += 1;
            if stats.skipped_kinds[0] < u32::MAX {
                stats.skipped_kinds[0] += 1;
            }
        }
        _ => {}
    }
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
        assert_eq!(st.skipped, 1);
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
