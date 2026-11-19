//! loader.rs — фоновая загрузка чертежа.
//!
//! Почему отдельный поток: чтение файла с сетевого диска и разбор большого DXF
//! занимают секунды, иногда десятки. В потоке интерфейса это выглядело как
//! «программа не отвечает».
//!
//! Почему отдельный поток ЕЩЁ и для чтения: `read` на мёртвой SMB-шаре блокируется
//! навсегда, и загрузчик не мог ни отмениться, ни сообщить о проблеме — окно
//! показывало «чтение 66%» и висело. Теперь чтение идёт своим потоком и отдаёт
//! куски через канал, а загрузчик ждёт их с таймаутом: отмена срабатывает сразу,
//! а отсутствие данных дольше STALL даёт внятную ошибку вместо тишины.
//!
//! Границы названы прямо: разбор не прерывается посреди одной сущности (проверка
//! отмены — раз в 8192 тега), файлы больше `MAX_FILE` не читаются вовсе, а
//! развёрнутых объектов не больше `MAX_ENTITIES`.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::doc::Doc;
use crate::dxf::{self, ParseStats};

/// Больше этого не читаем: 1.5 ГБ DXF — это уже не «чертёж», а выгрузка базы.
pub const MAX_FILE: u64 = 1_500_000_000;
/// Потолок развёрнутых блоков: защита от чертежа, где вставки дают десятки
/// миллионов примитивов.
pub const MAX_ENTITIES: u64 = 4_000_000;
/// Сколько ждём данных, прежде чем сказать, что чтение встало.
pub const STALL: Duration = Duration::from_secs(15);

/// Атрибуты файла Windows: нужны, чтобы отличить «медленный диск» от облачной
/// заготовки (OneDrive/Яндекс.Диск). Такие файлы локально выглядят обычными, но
/// первое чтение ждёт скачивания — именно это и выглядит как «висит на 66%».
#[cfg(windows)]
fn file_attributes(path: &str) -> u32 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileAttributesW(name: *const u16) -> u32;
    }
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    unsafe { GetFileAttributesW(wide.as_ptr()) }
}

#[cfg(not(windows))]
fn file_attributes(_path: &str) -> u32 {
    0
}

const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

/// Облачная заготовка: данные физически не на этом диске.
pub fn is_cloud_placeholder(path: &str) -> bool {
    let a = file_attributes(path);
    a != INVALID_FILE_ATTRIBUTES
        && (a & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)) != 0
}

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
    /// Байт в секунду (сглаженно).
    pub rate: Arc<AtomicU64>,
    /// Файл — облачная заготовка (данные ещё не скачаны).
    pub cloud: Arc<AtomicBool>,
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
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }
    pub fn cloud(&self) -> bool {
        self.cloud.load(Ordering::Relaxed)
    }
    pub fn set_cloud(&self, v: bool) {
        self.cloud.store(v, Ordering::Relaxed);
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

fn fail(path: &str, msg: String) -> Loaded {
    Loaded { path: path.to_string(), doc: Err(msg), stats: ParseStats::default() }
}

/// Чтение файла отдельным потоком, с таймаутом на данные и отменой.
fn read_all(path: &str, prog: &Progress) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let (tx, rx) = channel::<Result<Vec<u8>, String>>();
    let p = prog.clone();
    let path2 = path.to_string();
    std::thread::spawn(move || {
        let mut f = match std::fs::File::open(&path2) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err(format!(
                    "не открывается: {} (код {})",
                    crate::ports::describe_io(&e),
                    e.raw_os_error().unwrap_or(0)
                )));
                return;
            }
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        p.total.store(len.max(1), Ordering::Relaxed);
        if len > MAX_FILE {
            let _ = tx.send(Err(format!(
                "файл {} МБ больше предела {} МБ — откройте его в AutoCAD и сохраните нужный фрагмент",
                len / 1_048_576,
                MAX_FILE / 1_048_576
            )));
            return;
        }
        if len == 0 {
            let _ = tx.send(Err("файл пустой (0 байт)".to_string()));
            return;
        }
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Ok(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("ошибка чтения: {}", crate::ports::describe_io(&e))));
                    return;
                }
            }
        }
        let _ = tx.send(Ok(Vec::new())); // маркер конца
    });

    let mut data: Vec<u8> = Vec::new();
    let mut last_data = Instant::now();
    let mut mark = Instant::now();
    let mut mark_bytes = 0u64;
    loop {
        if prog.cancelled() {
            return Err("загрузка отменена".to_string());
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(chunk)) if chunk.is_empty() => break,
            Ok(Ok(chunk)) => {
                data.extend_from_slice(&chunk);
                prog.done.store(data.len() as u64, Ordering::Relaxed);
                last_data = Instant::now();
                let el = mark.elapsed();
                if el >= Duration::from_millis(400) {
                    let bps = ((data.len() as u64 - mark_bytes) as f64 / el.as_secs_f64()) as u64;
                    // сглаживание: 70% старого значения
                    let prev = prog.rate.load(Ordering::Relaxed);
                    let smoothed = if prev == 0 { bps } else { (prev / 10) * 7 + bps / 10 * 3 };
                    prog.rate.store(smoothed, Ordering::Relaxed);
                    mark = Instant::now();
                    mark_bytes = data.len() as u64;
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if last_data.elapsed() > STALL {
                    let cloud_hint = if prog.cloud() {
                        " Файл — облачная заготовка (OneDrive/Яндекс.Диск и подобное): данные ещё не на диске, \
                         первое чтение ждёт скачивания. Откройте файл один раз в проводнике и повторите."
                    } else {
                        ""
                    };
                    return Err(format!(
                        "чтение встало: {} с без данных (прочитано {} из {} МБ). \
                         Обычные причины: файл проверяет антивирус; файл — облачная заготовка; \
                         диск занят или отключился; путь указывает на отключённый сетевой диск.{}",
                        STALL.as_secs(),
                        data.len() / 1_048_576,
                        prog.total() / 1_048_576,
                        cloud_hint
                    ));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(data)
}

fn load(path: &str, prog: &Progress) -> Loaded {
    prog.phase.store(PHASE_READ, Ordering::Relaxed);
    prog.set_cloud(is_cloud_placeholder(path));
    let data = match read_all(path, prog) {
        Ok(d) => d,
        Err(e) => return fail(path, e),
    };
    // --- разбор
    prog.phase.store(PHASE_PARSE, Ordering::Relaxed);
    prog.total.store(data.len().max(1) as u64, Ordering::Relaxed);
    prog.done.store(0, Ordering::Relaxed);
    prog.rate.store(0, Ordering::Relaxed);
    let mut doc = Doc::with_capacity(4096);
    let mut stats = ParseStats::default();
    match dxf::parse_with(&data, &mut doc, Some(prog), &mut stats) {
        Ok(_) => {
            doc.path.push_str(path);
            prog.done.store(data.len() as u64, Ordering::Relaxed);
            Loaded { path: path.to_string(), doc: Ok(doc), stats }
        }
        Err(dxf::DxfError::Cancelled) => fail(path, "загрузка отменена".to_string()),
        Err(e) => fail(path, format!("не разобран как DXF: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str, bytes: &[u8]) -> String {
        let dir = std::env::temp_dir();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
    }

    #[test]
    fn reads_whole_file() {
        let data = vec![b'A'; 3 * 1_048_576 + 137];
        let p = tmp_file("dxfsign_loader_ok.bin", &data);
        let prog = Progress::new();
        let got = read_all(&p, &prog).expect("файл читается");
        assert_eq!(got.len(), data.len());
        assert_eq!(prog.done(), data.len() as u64);
        assert!(prog.percent() >= 99, "прогресс дошёл до конца: {}", prog.percent());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn cancel_takes_effect_before_reading() {
        let p = tmp_file("dxfsign_loader_cancel.bin", &vec![0u8; 1_048_576]);
        let prog = Progress::new();
        prog.cancel();
        let e = read_all(&p, &prog).unwrap_err();
        assert!(e.contains("отменена"), "ожидалась отмена, получено: {}", e);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_file_gives_reason() {
        let prog = Progress::new();
        let e = read_all(r"C:\__dxfsign_no_such_dir__\nope.dxf", &prog).unwrap_err();
        assert!(e.contains("не открывается"), "сообщение: {}", e);
    }

    #[test]
    fn empty_file_is_reported() {
        let p = tmp_file("dxfsign_loader_empty.dxf", b"");
        let prog = Progress::new();
        let e = read_all(&p, &prog).unwrap_err();
        assert!(e.contains("пустой"), "сообщение: {}", e);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn cloud_flag_does_not_panic_on_normal_file() {
        let p = tmp_file("dxfsign_loader_plain.dxf", b"0\nEOF\n");
        let _ = is_cloud_placeholder(&p);
        let _ = std::fs::remove_file(&p);
    }
}
