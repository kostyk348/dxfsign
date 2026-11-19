//! journal.rs — журнал правок как отдельный файл между шагами `edit` и `save`.
//!
//! Зачем отдельный файл: в скелете правка и сохранение — две команды (так каждый
//! шаг проверяется отдельно), а `Doc.log` живёт в памяти процесса. Без сайдкара
//! перечень изменений терялся: `.txt` и `.eml` уходили с «правок: 0». Это был
//! реальный дефект сквозного прогона, пойманый на живом exe, а не в теории.
//!
//! Формат: TSV, по строке на правку, поля percent-экранированы (`%09`, `%0A`, `%0D`,
//! `%25`). Текстовый формат выбран, чтобы журнал можно было прочитать глазами и
//! чтобы его не ломал ни один редактор. Потребляется он при `save` и удаляется:
//! проведённая через версии правка не должна применяться дважды.

use crate::doc::{Change, ChangeKind, Doc};
use crate::pool::Handle;
use crate::text::StrRef;

const MAGIC: &str = "dxfsign-journal	1";

fn esc(s: &[u8], out: &mut String) {
    out.clear();
    for &b in s {
        match b {
            b'%' => out.push_str("%25"),
            b'\t' => out.push_str("%09"),
            b'\n' => out.push_str("%0A"),
            b'\r' => out.push_str("%0D"),
            _ => {
                // байты вне ASCII пишем как %XX, чтобы файл оставался валидным UTF-8
                if b < 0x80 {
                    out.push(b as char);
                } else {
                    out.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
}

fn unesc(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn kind_code(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::TextEdited => "T",
        ChangeKind::TextMoved => "P",
        ChangeKind::TextHeight => "H",
        ChangeKind::LineMoved => "L",
        ChangeKind::LineAdded => "LA",
        ChangeKind::LineDeleted => "LD",
        ChangeKind::CircleEdited => "C",
        ChangeKind::PolyEdited => "W",
        ChangeKind::Comment => "M",
    }
}

fn code_kind(c: &str) -> Option<ChangeKind> {
    Some(match c {
        "T" => ChangeKind::TextEdited,
        "P" => ChangeKind::TextMoved,
        "H" => ChangeKind::TextHeight,
        "L" => ChangeKind::LineMoved,
        "LA" => ChangeKind::LineAdded,
        "LD" => ChangeKind::LineDeleted,
        "C" => ChangeKind::CircleEdited,
        "W" => ChangeKind::PolyEdited,
        "M" => ChangeKind::Comment,
        _ => return None,
    })
}

/// Записать журнал рядом с чертежомом (`<путь>.journal.tsv`). O(K), K ≤ 1024.
pub fn write(doc: &Doc, path: &str) -> std::io::Result<()> {
    let mut s = String::with_capacity(64 + doc.log.len() * 96);
    s.push_str(MAGIC);
    s.push('\n');
    let mut a = String::new();
    let mut b = String::new();
    let mut c = String::new();
    for ch in &doc.log.entries {
        s.push_str(kind_code(ch.kind));
        s.push('\t');
        esc(doc.arena_get(ch.old), &mut a);
        s.push_str(&a);
        s.push('\t');
        esc(doc.arena_get(ch.new), &mut b);
        s.push_str(&b);
        s.push('\t');
        esc(doc.arena_get(ch.wire), &mut c);
        s.push_str(&c);
        s.push('\t');
        esc(doc.arena_get(ch.comment), &mut c);
        s.push_str(&c);
        s.push_str(&format!("\t{}\t{}", ch.old_num, ch.new_num));
        s.push('\n');
    }
    std::fs::write(path, s)
}

/// Прочитать журнал в документ (строки уходят в арену документа). O(K).
pub fn read(doc: &mut Doc, path: &str) -> std::io::Result<usize> {
    let text = std::fs::read_to_string(path)?;
    let mut n = 0usize;
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            continue; // MAGIC
        }
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            continue;
        }
        let Some(kind) = code_kind(f[0]) else { continue };
        let old = doc.arena.push(&unesc(f[1])).0;
        let new = doc.arena.push(&unesc(f[2])).0;
        let wire = doc.arena.push(&unesc(f[3])).0;
        let comment = doc.arena.push(&unesc(f[4])).0;
        doc.log.push(Change {
            kind,
            entity: Handle::NONE,
            old,
            new,
            wire,
            comment,
            old_num: f[5].parse().unwrap_or(0.0),
            new_num: f[6].parse().unwrap_or(0.0),
        });
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Doc, TextEnt, TEXT_KIND_TEXT};
    use crate::text::StrRef;

    fn doc() -> Doc {
        let mut d = Doc::with_capacity(8);
        let layer = d.layer_index("0".as_bytes());
        let p = d.push_point(0.0, 0.0);
        let (s, _) = d.arena.push(&"Клемма 1".as_bytes().to_vec());
        d.texts.push(TextEnt {
            pos: p,
            height: 2.5,
            rot_deg: 0.0,
            layer,
            kind: TEXT_KIND_TEXT,
            s,
            wire: StrRef::EMPTY,
            dxf_handle: 1,
        });
        d.handles.push(Handle { slot: 0, gen: 1 });
        d
    }

    #[test]
    fn journal_roundtrip_keeps_cyrillic_wire_and_comment() {
        let mut d = doc();
        d.edit_text(0, "Клемма 2".as_bytes(), b"W-101", "по замечанию ПТО".as_bytes());
        d.add_comment(0, "проверить сечение".as_bytes(), b"W-102");
        let dir = std::env::temp_dir().join(format!("dxfsign-j-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.journal.tsv");
        write(&d, &p.to_string_lossy()).unwrap();
        let mut d2 = doc();
        let n = read(&mut d2, &p.to_string_lossy()).unwrap();
        assert_eq!(n, 2);
        assert_eq!(d2.log.len(), 2);
        assert_eq!(d2.arena_get(d2.log.entries[0].old), "Клемма 1".as_bytes());
        assert_eq!(d2.arena_get(d2.log.entries[0].new), "Клемма 2".as_bytes());
        assert_eq!(d2.arena_get(d2.log.entries[0].wire), b"W-101");
        assert_eq!(d2.arena_get(d2.log.entries[0].comment), "по замечанию ПТО".as_bytes());
        assert_eq!(d2.log.entries[1].kind, ChangeKind::Comment);
        assert_eq!(d2.arena_get(d2.log.entries[1].comment), "проверить сечение".as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
