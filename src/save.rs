//! save.rs — протокол сохранения. Четыре шага, все обязательные, порядок важен.
//!
//!   1. Собрать новые байты DXF в памяти и посчитать их SHA-256.
//!   2. Прочитать текущий файл с диска (это «прежняя версия») и, если чертёж
//!      изменился, ПОЛОЖИТЬ ПРЕЖНИЕ БАЙТЫ В ПОДПАПКУ ВЕРСИЙ
//!      (`_VERSIONS/<имя>__<YYYYMMDD-HHMMSS>_<seq>.dxf`).
//!   3. Перезаписать РАБОЧИЙ файл атомарно (временный + rename): тот, кто читает
//!      сетевой диск, видит либо старый файл, либо новый, но не половину.
//!   4. Записать `.txt` с перечнем правок (рядом с чертежом), собрать `.eml`
//!      с тремя вложениями и подписью, положить его в спул, дописать якорь
//!      в цепочку версий.
//!
//! Гарантия при сбое: до шага 3 рабочий файл не тронут; если падает шаг 3,
//! версия уже лежит в `_VERSIONS` (то есть ничего не потеряно, повтор возможен);
//! если падает шаг 4 — файлы на диске записаны, а якорь цепи письма не создан,
//! и это видно по расхождению `head` в индексе с последним письмом в спуле.

use crate::doc::{Change, Doc};
use crate::eml::{self, Attachment, EmlMeta};
use crate::ports::{FileSource, MailTransport, Signer, VersionRecord, VersionStore};
use crate::sha256::{hex, sha256};

#[derive(Clone, Debug)]
pub struct SaveConfig {
    pub from_addr: String,
    pub to_addr: String,
    /// Имя подпапки версий рядом с чертежом. ТЗ: «старый отправляется в подпапку
    /// версии»; имя не задано — принято `_VERSIONS` (совместимо с наименованием
    /// `_OMNI_HISTORY` из cadcraft-ветки omni-drawing-history).
    pub versions_dir: String,
    pub spool_dir: String,
    /// Смещение пояса в минутах для дат в письме и в `.txt` (MSK = 180).
    /// Часовой пояс берётся из конфига, а не из системы: на терминале цеха
    /// локали могут отличаться, а в письме нужна одна шкала.
    pub tz_offset_min: i32,
    /// Сколько файлов версий держать; 0 — не чистить (решение за владельцем).
    pub keep_versions: usize,
}

impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            from_addr: "dxfsign@localhost".to_string(),
            to_addr: String::new(),
            versions_dir: "_VERSIONS".to_string(),
            spool_dir: "spool".to_string(),
            tz_offset_min: 180,
            keep_versions: 0,
        }
    }
}

#[derive(Debug)]
pub enum SaveError {
    Io(String),
    /// Нет прав на запись. Владелец должен видеть, куда именно и почему.
    AccessDenied { path: String, hint: String },
    NothingChanged,
}

impl SaveError {
    /// Ошибка, при которой имеет смысл предложить сохранить в другое место.
    pub fn is_access(&self) -> bool {
        matches!(self, SaveError::AccessDenied { .. })
    }
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Io(s) => write!(f, "{}", s),
            SaveError::AccessDenied { path, hint } => {
                if hint.contains(path.as_str()) {
                    write!(f, "отказано в доступе: {}", hint)
                } else {
                    write!(f, "отказано в доступе: {} — {}", path, hint)
                }
            }
            SaveError::NothingChanged => write!(f, "чертёж не менялся: версия не пишется"),
        }
    }
}

#[derive(Debug, Default)]
pub struct SaveOutcome {
    pub new_hex: String,
    pub prev_hex: String,
    pub version_file: String,
    pub txt_path: String,
    pub eml_path: String,
    pub edits: u32,
    pub wires: String,
    pub written: bool,
}

// ---------------------------------------------------------------- дата/время

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Говард Хиннант, «chrono-compatible low-level date algorithms»
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `(YYYY-MM-DD, HH:MM:SS)` в заданном поясе. O(1).
pub fn civil_time(unix_secs: i64, tz_offset_min: i32) -> (String, String) {
    let local = unix_secs + (tz_offset_min as i64) * 60;
    let days = local.div_euclid(86400);
    let secs = local.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    (
        format!("{:04}-{:02}-{:02}", y, m, d),
        format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60),
    )
}

/// Компактная метка для имени файла: `20261119-113500`. O(1).
pub fn stamp_compact(unix_secs: i64, tz_offset_min: i32) -> String {
    let (d, t) = civil_time(unix_secs, tz_offset_min);
    format!("{}-{}", d.replace('-', ""), t.replace(':', ""))
}

fn rfc5322_date(unix_secs: i64, tz_offset_min: i32) -> String {
    let (d, t) = civil_time(unix_secs, tz_offset_min);
    let days = (unix_secs + tz_offset_min as i64 * 60).div_euclid(86400);
    // день недели: 1970-01-01 был четвергом (4)
    let wd = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][((days % 7 + 7 + 4) % 7) as usize];
    let mon = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let m: usize = d[5..7].parse().unwrap_or(1);
    let sign = if tz_offset_min < 0 { '-' } else { '+' };
    let off = tz_offset_min.abs();
    format!(
        "{}, {} {} {} {} {}{:02}{:02}",
        wd,
        &d[8..10],
        mon[m - 1],
        &d[0..4],
        &t[0..8],
        sign,
        off / 60,
        off % 60
    )
}

// ---------------------------------------------------------------- имя файла

fn file_stem(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// Каталог файла (полный, а не последний сегмент): на нём строятся пути
/// `_VERSIONS` и индекса цепочки. O(len).
pub fn parent_dir(path: &str) -> String {
    match path.rfind(['\\', '/']) {
        Some(i) if i > 0 => path[..i].to_string(),
        _ => ".".to_string(),
    }
}

pub fn join(dir: &str, name: &str) -> String {
    let sep = if dir.contains('/') && !dir.contains('\\') { '/' } else { '\\' };
    format!("{}{}{}", dir.trim_end_matches(['\\', '/']), sep, name)
}

// ---------------------------------------------------------------- перечень правок

/// Собрать `.txt`: человеку, а не парсеру. O(K) по числу правок.
pub fn build_changelog(
    doc: &Doc,
    cfg: &SaveConfig,
    unix_secs: i64,
    seq: u64,
    new_hex: &str,
    prev_hex: &str,
    version_file: &str,
) -> String {
    let (d, t) = civil_time(unix_secs, cfg.tz_offset_min);
    let mut s = String::with_capacity(512 + doc.log.len() * 160);
    s.push_str("dxfsign — перечень изменений чертежа\n");
    s.push_str("=========================================================\n");
    s.push_str(&format!("Файл:        {}\n", doc.path));
    s.push_str(&format!("Время:       {} {} (UTC{:+03}:{:02})\n", d, t, cfg.tz_offset_min / 60, cfg.tz_offset_min.abs() % 60));
    s.push_str(&format!("Версия:      v{}\n", seq));
    s.push_str(&format!("Правок:      {}\n", doc.log.len()));
    s.push_str(&format!("SHA-256 новый: {}\n", new_hex));
    s.push_str(&format!("SHA-256 прежний: {}\n", if prev_hex.is_empty() { "— (файла не было)" } else { prev_hex }));
    s.push_str(&format!("Прежняя версия:  {}\n", if version_file.is_empty() { "—" } else { version_file }));
    s.push_str("---------------------------------------------------------\n");
    s.push_str("№  тип              провод      было / стало / комментарий\n");
    for (i, c) in doc.log.entries.iter().enumerate() {
        s.push_str(&format!("{:<3}{:<17}{:<13}", i + 1, c.kind.label(), doc.arena_utf8(c.wire)));
        match c.kind {
            crate::doc::ChangeKind::TextEdited => {
                s.push_str(&format!(
                    "\"{}\" -> \"{}\"\n",
                    doc.arena_utf8(c.old),
                    doc.arena_utf8(c.new)
                ));
            }
            crate::doc::ChangeKind::TextHeight => {
                s.push_str(&format!("height {} -> {}\n", c.old_num, c.new_num));
            }
            crate::doc::ChangeKind::Comment => {
                s.push_str(&format!("комментарий: \"{}\"\n", doc.arena_utf8(c.comment)));
            }
            _ => {
                s.push_str(&format!("{} -> {}\n", c.old_num, c.new_num));
            }
        }
        // Комментарий печатается для любой записи, где он задан: владелец хотел
        // видеть свободный текст рядом с правкой, а не отдельным приложением.
        if c.kind != crate::doc::ChangeKind::Comment && !c.comment.is_empty() {
            s.push_str(&format!("    комментарий: \"{}\"\n", doc.arena_utf8(c.comment)));
        }
    }
    s.push_str("---------------------------------------------------------\n");
    let wires = collect_wires(&doc.log.entries, doc);
    s.push_str(&format!("Номера проводов: {}\n", if wires.is_empty() { "—".into() } else { wires }));
    let comments = collect_comments(&doc.log.entries, doc);
    s.push_str(&format!("Комментарии:     {}\n", if comments.is_empty() { "—".into() } else { comments }));
    s.push_str("Подпись: HMAC-SHA256 в .eml (X-DXFSIGN-Signature)\n");
    s
}

/// Уникальные номера проводов в порядке появления. O(K²) на малом K (K — правки,
/// ограничены 1024), поэтому без хеш-таблицы: 1024² сравнений — это ~1 мс, а
/// аллокаций ноль. На 10× данных (10 000 правок) станет 10⁸ — там уже нужна
/// сортировка: см. «что делать на 10×» в отчёте.
pub fn collect_wires(entries: &[Change], doc: &Doc) -> String {
    let mut seen: Vec<&[u8]> = Vec::with_capacity(8);
    let mut out = String::with_capacity(64);
    for c in entries {
        let w = doc.arena_get(c.wire);
        if w.is_empty() || seen.contains(&w) {
            continue;
        }
        seen.push(w);
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&String::from_utf8_lossy(w));
    }
    out
}

/// Свободные комментарии из журнала, в порядке появления, через ` | `.
/// Комментарии не дедуплицируются: две одинаковые фразы от разных правок —
/// это две записи, а не одна.
pub fn collect_comments(entries: &[Change], doc: &Doc) -> String {
    let mut out = String::with_capacity(128);
    for c in entries {
        let t = doc.arena_utf8(c.comment);
        if t.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&t);
    }
    out
}

// ---------------------------------------------------------------- протокол

/// Полный цикл сохранения. O(N + B) где N — сущности, B — размер файла;
/// на диск идёт 2 записи файла + 1 письмо + 1 якорь. Синхронно, вне кадра.
pub fn save(
    doc: &Doc,
    cfg: &SaveConfig,
    fs: &mut dyn FileSource,
    signer: &dyn Signer,
    mail: &mut dyn MailTransport,
    store: &mut dyn VersionStore,
    unix_secs: i64,
    seq: u64,
    dxf_bytes: &mut Vec<u8>,
) -> Result<SaveOutcome, SaveError> {
    save_to(doc, cfg, fs, signer, mail, store, unix_secs, seq, dxf_bytes, None)
}

/// То же сохранение, но рабочий файл — `forced_target` (когда в родной каталог
/// писать нельзя: нет прав, файл read-only, сетевой диск недоступен).
#[allow(clippy::too_many_arguments)]
pub fn save_to(
    doc: &Doc,
    cfg: &SaveConfig,
    fs: &mut dyn FileSource,
    signer: &dyn Signer,
    mail: &mut dyn MailTransport,
    store: &mut dyn VersionStore,
    unix_secs: i64,
    seq: u64,
    dxf_bytes: &mut Vec<u8>,
    forced_target: Option<&str>,
) -> Result<SaveOutcome, SaveError> {
    let doc_path: String = forced_target.map(|s| s.to_string()).unwrap_or_else(|| doc.path.clone());
    // ---- шаг 1: новые байты и их хеш
    let _lost = crate::dxf::write(doc, dxf_bytes);
    let new_hash = sha256(dxf_bytes);
    let mut out = SaveOutcome {
        edits: doc.log.len() as u32,
        ..Default::default()
    };
    hex(&new_hash, &mut out.new_hex);

    // ---- шаг 2: прежняя версия -> подпапка версий
    let prev = if fs.exists(&doc_path) {
        Some(fs.read(&doc_path).map_err(|e| SaveError::Io(e.to_string()))?)
    } else {
        None
    };
    let prev_hash = prev.as_ref().map(|b| sha256(b));
    if let Some(ph) = &prev_hash {
        hex(ph, &mut out.prev_hex);
    }
    let changed = match &prev_hash {
        None => true,
        Some(ph) => *ph != new_hash,
    };
    if !changed {
        return Err(SaveError::NothingChanged);
    }

    let dir = parent_dir(&doc_path);
    let vdir = join(&dir, &cfg.versions_dir);
    // Права проверяем ДО первой записи: иначе человек получает «os error 5» уже
    // после того, как часть протокола отработала.
    if fs.exists(&dir) {
        crate::ports::probe_writable(fs, &dir).map_err(|hint| SaveError::AccessDenied { path: dir.clone(), hint })?;
    } else {
        return Err(SaveError::Io(format!("каталог чертежа не найден: {}", dir)));
    }
    // файл может быть помечен «только чтение» — это тоже error 5
    if let Ok(md) = std::fs::metadata(&doc_path) {
        if md.permissions().readonly() {
            return Err(SaveError::AccessDenied {
                path: doc_path.clone(),
                hint: "файл помечен «только чтение»: снимите атрибут или сохраните через «Сохранить как»".to_string(),
            });
        }
    }
    if let Some(pb) = &prev {
        fs.create_dir_all(&vdir).map_err(|e| {
        if e.raw_os_error() == Some(5) {
            SaveError::AccessDenied { path: vdir.clone(), hint: crate::ports::describe_io(&e) }
        } else {
            SaveError::Io(format!("каталог версий «{}»: {}", vdir, crate::ports::describe_io(&e)))
        }
    })?;
        let vname = format!(
            "{}__{}_{:03}.dxf",
            file_stem(&doc_path),
            stamp_compact(unix_secs, cfg.tz_offset_min),
            seq
        );
        let vpath = join(&vdir, &vname);
        fs.write_atomic(&vpath, pb).map_err(|e| SaveError::Io(e.to_string()))?;
        // Проверка, что в подпапку легли ровно те байты: на SMB бывают
        // укороченные записи, и молча принять это нельзя.
        let back = fs.read(&vpath).map_err(|e| SaveError::Io(e.to_string()))?;
        if sha256(&back) != *prev_hash.as_ref().unwrap() {
            return Err(SaveError::Io(format!(
                "версия {} записалась с другим хешем — сохранение остановлено",
                vpath
            )));
        }
        out.version_file = vpath;
    }

    // ---- шаг 3: перезапись рабочего файла
    fs.write_atomic(&doc_path, dxf_bytes)
        .map_err(|e| SaveError::Io(format!("запись «{}»: {}", doc_path, crate::ports::describe_io(&e))))?;
    out.written = true;

    // ---- шаг 4a: .txt рядом с чертежом
    let txt_name = format!(
        "{}__{}.changes.txt",
        file_stem(&doc_path),
        stamp_compact(unix_secs, cfg.tz_offset_min)
    );
    let txt_path = join(&dir, &txt_name);
    let changelog = build_changelog(
        doc,
        cfg,
        unix_secs,
        seq,
        &out.new_hex,
        &out.prev_hex,
        &out.version_file,
    );
    fs.write_atomic(&txt_path, changelog.as_bytes())
        .map_err(|e| SaveError::Io(e.to_string()))?;
    out.txt_path = txt_path;

    // ---- шаг 4b: письмо
    let wires = collect_wires(&doc.log.entries, doc);
    out.wires = wires.clone();
    let comments = collect_comments(&doc.log.entries, doc);
    let canon = eml::canonical(
        &doc_path,
        &out.new_hex,
        &out.prev_hex,
        &out.version_file,
        &wires,
        out.edits,
        unix_secs,
    );
    let sig = signer.sign(canon.as_bytes());
    let mut x_headers: Vec<(String, String)> = Vec::with_capacity(10);
    x_headers.push(("X-DXFSIGN-Working-File".into(), doc_path.clone()));
    x_headers.push(("X-DXFSIGN-New-SHA256".into(), out.new_hex.clone()));
    x_headers.push(("X-DXFSIGN-Prev-SHA256".into(), out.prev_hex.clone()));
    x_headers.push(("X-DXFSIGN-Prev-Version".into(), out.version_file.clone()));
    x_headers.push(("X-DXFSIGN-Wires".into(), wires.clone()));
    x_headers.push(("X-DXFSIGN-Comments".into(), comments.clone()));
    x_headers.push(("X-DXFSIGN-Edits".into(), out.edits.to_string()));
    x_headers.push(("X-DXFSIGN-ChangeLog".into(), txt_name.clone()));
    x_headers.push((
        "X-DXFSIGN-Chain-Head".into(),
        store.head().map(|h| {
            let mut s = String::new();
            hex(&h, &mut s);
            s
        }).unwrap_or_else(|| "genesis".into()),
    ));
    match &sig {
        Some(m) => {
            let mut h = String::new();
            hex(m, &mut h);
            x_headers.push((
                "X-DXFSIGN-Signature".into(),
                format!("HMAC-SHA256 key={}; h={}", signer.key_id(), h),
            ));
        }
        None => x_headers.push(("X-DXFSIGN-Unsigned".into(), "ключ подписи не задан".into())),
    }

    let subject = format!(
        "[DXFSIGN] {} — правок: {} — провод: {}",
        file_stem(&doc_path),
        out.edits,
        if wires.is_empty() { "—".to_string() } else { wires.clone() }
    );
    let meta = EmlMeta {
        from: &cfg.from_addr,
        to: if cfg.to_addr.is_empty() { "[REDACTED]" } else { &cfg.to_addr },
        subject,
        date_rfc5322: rfc5322_date(unix_secs, cfg.tz_offset_min),
        message_id: eml::message_id(unix_secs, seq),
        body_text: format!(
            "Сохранение чертежа.\n\nФайл: {}\nПравок: {}\nНомера проводов: {}\nКомментарии: {}\nПрежняя версия: {}\nПеречень изменений: {}\n\nФайлы во вложении: новый .dxf, прежний .dxf, перечень .txt.\n",
            doc_path,
            out.edits,
            if wires.is_empty() { "—".to_string() } else { wires.clone() },
            if comments.is_empty() { "—".to_string() } else { comments.clone() },
            if out.version_file.is_empty() { "—".to_string() } else { out.version_file.clone() },
            txt_name
        ),
        x_headers,
    };
    let atts = vec![
        Attachment {
            filename: format!("{}.dxf", file_stem(&doc_path)),
            mime: "application/octet-stream",
            data: dxf_bytes.as_slice(),
        },
        Attachment {
            filename: format!("prev__{}.dxf", file_stem(&doc_path)),
            mime: "application/octet-stream",
            data: prev.as_deref().unwrap_or(&[]),
        },
        Attachment {
            filename: txt_name.clone(),
            mime: "text/plain",
            data: changelog.as_bytes(),
        },
    ];
    let mut raw = Vec::with_capacity(dxf_bytes.len() * 2 + changelog.len() * 2 + 4096);
    eml::build(&meta, &atts, seq ^ (unix_secs as u64), &mut raw);
    let eml_name = format!(
        "{}__{}.eml",
        file_stem(&doc_path),
        stamp_compact(unix_secs, cfg.tz_offset_min)
    );
    let eml_path = mail
        .deliver(&raw, &eml_name)
        .map_err(|e| SaveError::Io(e.to_string()))?;
    out.eml_path = eml_path;

    // ---- шаг 4c: якорь в цепочке
    let mut chain_input = Vec::with_capacity(96);
    chain_input.extend_from_slice(&new_hash);
    chain_input.extend_from_slice(&prev_hash.unwrap_or([0u8; 32]));
    chain_input.extend_from_slice(&store.head().unwrap_or([0u8; 32]));
    let chain_hash = sha256(&chain_input);
    let rec = VersionRecord {
        seq,
        unix_secs,
        working_file: doc_path.clone(),
        version_file: out.version_file.clone(),
        new_hash,
        prev_hash: prev_hash.unwrap_or([0u8; 32]),
        chain_hash,
        wires: wires.clone(),
        edits: out.edits,
    };
    store.append(&rec).map_err(|e| SaveError::Io(e.to_string()))?;

    // ---- шаг 4d: ротация версий (только если владелец задал предел)
    if cfg.keep_versions > 0 {
        if let Ok(mut list) = fs.list(&vdir) {
            let mut files: Vec<_> = list.drain(..).filter(|e| !e.is_dir).collect();
            files.sort_by_key(|e| e.name.clone());
            while files.len() > cfg.keep_versions {
                let victim = files.remove(0);
                let _ = fs.remove_file(&join(&vdir, &victim.name));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_time_known_values() {
        // 2026-11-19T08:35:00Z == 11:35 MSK
        let ts = 1795077300;
        let (d, t) = civil_time(ts, 180);
        assert_eq!(d, "2026-11-19");
        assert_eq!(t, "11:35:00");
        assert_eq!(stamp_compact(ts, 180), "20261119-113500");
    }

    #[test]
    fn rfc5322_has_zone_and_weekday() {
        let s = rfc5322_date(1795077300, 180);
        assert!(s.contains("+0300"), "{}", s);
        assert!(s.ends_with("+0300"));
        assert!(s.split(',').next().unwrap().len() == 3);
    }

    #[test]
    fn stem_and_parent_for_windows_paths() {
        assert_eq!(file_stem(r"Z:\obj\itp-3\plan.dxf"), "plan");
        assert_eq!(parent_dir(r"Z:\obj\itp-3\plan.dxf"), r"Z:\obj\itp-3");
        assert_eq!(join(r"Z:\obj\itp-3", "_VERSIONS"), r"Z:\obj\itp-3\_VERSIONS");
    }
}
