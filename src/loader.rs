//! loader.rs — фоновая загрузка чертежа.
//!
//! Зачем отдельный поток: чтение файла с сетевого диска и разбор большого DXF
//! занимают секунды, а иногда и десятки секунд. Делать это в потоке интерфейса
//! значит получить «программа не отвечает» — именно так выглядело «не грузит
//! вообще». Теперь окно продолжает жить, показывает прогресс (`%`, число
//! объектов) и позволяет отменить загрузку по Esc.
//!
//! Границы названы прямо: разбор не прерывается посреди одной сущности (проверка
//! отмены идёт раз в 8192 тега), и файлы больше `MAX_FILE` не читаются вовсе —
//! вместо зависания человек получает сообщение.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use crate::dxf::{self, ParseStats};
use crate::doc::Doc;

/// Больше этого не читаем: 1.5 ГБ DXF — это уже не «чертёж», а выгрузка базы.
pub const MAX_FILE: u64 = 1_500_000_000;
/// Потолок развёрнутых блоков: защита от чертежа, где 50 000 вставок вложенных
/// блоков дают десятки миллионов примитивов.
pub const MAX_ENTITIES: u64 = 4_000_000;

pub const PHASE_READ: u8 = 0;
pub const PHASE_PARSE: u8 = 1;

/// Разделяемый прогресс. Интерфейс читает атомики на каждой перерисовке — без
/// блокировок и без обмена сообщениями.
#[derive(Clone, Default)]
pub struct Progress {
    pub phase: Arc<AtomicU8>,
    pub done: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    pub entities: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn percent(&self) -> u32 {
        let total = self.total.load(Ordering::Relaxed).max(1);
        let done = self.done.load(Ordering::Relaxed).min(total);
        ((done as f64 / total as f64) * 100.0) as u32
    }
    pub fn phase(&self) -> u8 {
        self.phase.load(Ordering::Relaxed)
    }
    pub fn entities(&self) -> u64 {
        self.entities.load(Ordering::Relaxed)
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn reset_cancel(&self) {
        self.cancel.store(false, Ordering::Relaxed);
    }
}

pub struct Loaded {
    pub path: String,
    pub doc: Result<Doc, String>,
    pub stats: ParseStats,
}

/// Запустить загрузку в фоне. Возвращает прогресс (для отрисовки) и канал, из
/// которого окно забирает результат таймером.
pub fn spawn(path: String) -> (Progress, Receiver<Loaded>) {
    let (tx, rx) = channel();
    let prog = Progress::new();
    let p = prog.clone();
    std::thread::spawn(move || {
        let result = load(&path, &p);
        let _ = tx.send(result);
    });
    (prog, rx)
}

fn load(path: &str, prog: &Progress) -> Loaded {
    use std::io::Read;
    prog.phase.store(PHASE_READ, Ordering::Relaxed);
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return Loaded {
                path: path.to_string(),
                doc: Err(format!("не открывается: {} ({})", crate::ports::describe_io(&e), e.raw_os_error().unwrap_or(0))),
                stats: ParseStats::default(),
            }
        }
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    prog.total.store(len.max(1), Ordering::Relaxed);
    if len > MAX_FILE {
        return Loaded {
            path: path.to_string(),
            doc: Err(format!(
                "файл {} МБ больше предела {} МБ — откройте его в AutoCAD и сохраните нужный фрагмент",
                len / 1_048_576,
                MAX_FILE / 1_048_576
            )),
            stats: ParseStats::default(),
        };
    }
    if len == 0 {
        return Loaded { path: path.to_string(), doc: Err("файл пустой (0 байт)".into()), stats: ParseStats::default() };
    }
    let mut data = Vec::with_capacity(len as usize);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if prog.cancelled() {
            return Loaded { path: path.to_string(), doc: Err("загрузка отменена".into()), stats: ParseStats::default() };
        }
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                prog.done.fetch_add(n as u64, Ordering::Relaxed);
                if data.len() as u64 > MAX_FILE {
                    return Loaded { path: path.to_string(), doc: Err("файл слишком большой".into()), stats: ParseStats::default() };
                }
            }
            Err(e) => {
                return Loaded {
                    path: path.to_string(),
                    doc: Err(format!("ошибка чтения: {}", crate::ports::describe_io(&e))),
                    stats: ParseStats::default(),
                }
            }
        }
    }
    // --- разбор
    prog.phase.store(PHASE_PARSE, Ordering::Relaxed);
    prog.total.store(data.len().max(1) as u64, Ordering::Relaxed);
    prog.done.store(0, Ordering::Relaxed);
    let mut doc = Doc::with_capacity(4096);
    let mut stats = ParseStats::default();
    match dxf::parse_with(&data, &mut doc, Some(prog), &mut stats) {
        Ok(_) => {
            doc.path.push_str(path);
            prog.done.store(data.len() as u64, Ordering::Relaxed);
            Loaded { path: path.to_string(), doc: Ok(doc), stats }
        }
        Err(dxf::DxfError::Cancelled) => Loaded { path: path.to_string(), doc: Err("загрузка отменена".into()), stats },
        Err(e) => Loaded { path: path.to_string(), doc: Err(format!("не разобран как DXF: {}", e)), stats },
    }
}
