//! ports.rs — POP-слой: контракты на всё, что «снаружи» (файловый источник,
//! подписант, транспорт письма, хранилище версий).
//!
//! Разделение с DOP сознательное: массив геометрии не должен знать, лежит ли файл
//! на локальном диске, на SMB-шаре или в памяти теста. Поэтому горячий путь
//! (разбор, отрисовка, правка) работает с `Doc` напрямую, а всё медленное и
//! внешнее идёт через трейты — по одному вызову на сохранение, не в кадре.
//!
//! Цена честная: `dyn Trait` — виртуальный вызов (~2-5 нс) и невозможность
//! инлайна. Для операций «раз в сохранение» это ноль; там, где нужен инлайн,
//! используется статическая диспетчеризация (`impl Trait`), а не `dyn`.

use std::io;

/// Запись каталога для навигации по сетевому диску.
#[derive(Clone, Debug)]
pub struct DirEntryRec {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_unix: i64,
}

/// Источник файлов. Реализации: локальный/сетевой путь (UNC), память (тесты).
/// Проверка прав: пишем пробный файл. Без неё первая же ошибка возникает уже в
/// середине протокола сохранения, и человек видит только «os error 5».
pub fn probe_writable(fs: &mut dyn FileSource, dir: &str) -> Result<(), String> {
    let probe = format!("{}\\.dxfsign-write-test-{}", dir.trim_end_matches(['\\', '/']), std::process::id());
    match fs.write_atomic(&probe, b"ok") {
        Ok(()) => {
            let _ = fs.remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!("в каталоге «{}» {}", dir, describe_io(&e))),
    }
}

/// Человеческое описание ошибок ввода-вывода Windows. `os error 5` само по себе
/// ничего не говорит о том, что делать.
pub fn describe_io(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(5) => "отказано в доступе (нет прав на папку или файл помечен «только чтение»)".to_string(),
        Some(32) => "файл занят другой программой (закройте его в AutoCAD и повторите)".to_string(),
        Some(2) => "путь не найден (проверьте, что сетевой диск подключён)".to_string(),
        Some(3) => "путь не найден".to_string(),
        Some(19) => "диск защищён от записи".to_string(),
        Some(112) => "на диске нет места".to_string(),
        Some(123) => "недопустимое имя файла".to_string(),
        _ => e.to_string(),
    }
}

pub trait FileSource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
    fn exists(&self, path: &str) -> bool;
    /// Атомарная запись: временный файл рядом + rename. На SMB rename в пределах
    /// одного каталога атомарен, поэтому читатель не увидит половину файла.
    fn write_atomic(&mut self, path: &str, data: &[u8]) -> io::Result<()>;
    fn create_dir_all(&mut self, path: &str) -> io::Result<()>;
    /// Перемещение прежней версии в подпапку версий (создаёт подпапку при надобности).
    fn move_file(&mut self, from: &str, to: &str) -> io::Result<()>;
    fn list(&self, dir: &str) -> io::Result<Vec<DirEntryRec>>;
    /// Удаление файла версии при ротации. Отдельный метод, потому что удалять
    /// на сетевом диске — операция с последствиями, и её нельзя прятать в list().
    fn remove_file(&mut self, path: &str) -> io::Result<()>;
}

/// Подписант. Возвращает идентификатор ключа и MAC над канонической строкой.
/// `None` — подпись недоступна (нет ключа): тогда письмо помечается `X-DXFSIGN-Unsigned`,
/// а не подделывается молча.
pub trait Signer {
    fn sign(&self, canonical: &[u8]) -> Option<[u8; 32]>;
    fn key_id(&self) -> &str;
}

/// Транспорт письма. `SpoolTransport` кладёт `.eml` в каталог-спул (схема из
/// eml-scada: письмо — это и есть запись, агент доставки забирает файл);
/// `SmtpTransport` — отдельная реализация, в скелете НЕ реализована (см. отчёт).
pub trait MailTransport {
    /// Вернуть путь/идентификатор созданного письма.
    fn deliver(&mut self, raw: &[u8], suggested_name: &str) -> io::Result<String>;
}

/// Одна запись цепочки версий. Схема — из eml-scada/integrity.py:
/// каждая запись ссылается на хеш предыдущей, якорь фиксирует префикс истории.
#[derive(Clone, Debug)]
pub struct VersionRecord {
    pub seq: u64,
    pub unix_secs: i64,
    pub working_file: String,
    pub version_file: String,
    pub new_hash: [u8; 32],
    pub prev_hash: [u8; 32],
    pub chain_hash: [u8; 32],
    pub wires: String,
    pub edits: u32,
}

pub trait VersionStore {
    fn append(&mut self, rec: &VersionRecord) -> io::Result<()>;
    fn head(&self) -> Option<[u8; 32]>;
    fn count(&self) -> u64;
}

// ---------------------------------------------------------------- локальная ФС

/// Реализация поверх std::fs. Сетевой диск для этого кода — обычный путь
/// (`\\server\share\...` или `Z:\...`), отдельного API не требуется.
pub struct LocalFs;

impl LocalFs {
    pub const fn new() -> Self {
        LocalFs
    }
}

impl Default for LocalFs {
    fn default() -> Self {
        LocalFs::new()
    }
}

impl FileSource for LocalFs {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        let tmp = format!("{}.tmp{}", path, std::process::id());
        std::fs::write(&tmp, data)?;
        // std::fs::rename на Windows заменяет целевой файл атомарно
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    fn create_dir_all(&mut self, path: &str) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn move_file(&mut self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn list(&self, dir: &str) -> io::Result<Vec<DirEntryRec>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let md = e.metadata()?;
            let modified_unix = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(DirEntryRec {
                name: e.file_name().to_string_lossy().to_string(),
                is_dir: md.is_dir(),
                size: md.len(),
                modified_unix,
            });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }
}

// ---------------------------------------------------------------- подписант

/// HMAC-SHA256 с ключом из конфига. Ключ НЕ печатается и не логируется.
pub struct HmacSigner {
    key: Vec<u8>,
    id: String,
}

impl HmacSigner {
    pub fn new(key: Vec<u8>, id: String) -> Self {
        Self { key, id }
    }
    /// Ключа нет — подписи нет. Это честное состояние, а не «подписано пустым ключом».
    pub fn unavailable(id: String) -> Self {
        Self { key: Vec::new(), id }
    }
    pub fn has_key(&self) -> bool {
        !self.key.is_empty()
    }
}

impl Signer for HmacSigner {
    fn sign(&self, canonical: &[u8]) -> Option<[u8; 32]> {
        if self.key.is_empty() {
            return None;
        }
        Some(crate::sha256::hmac_sha256(&self.key, canonical))
    }
    fn key_id(&self) -> &str {
        &self.id
    }
}

// ---------------------------------------------------------------- спул писем

/// Спул: письмо пишется в каталог как `.eml` (атомарно, через FileSource).
/// Такой файл — готовое RFC 5322-сообщение: его забирает агент доставки,
/// кладёт в почтовый ящик, или подхватывает Outlook/Thunderbird как файл.
pub struct SpoolTransport {
    /// Собственный FileSource, а не заимствованный: иначе спул и хранилище
    /// версий дереут одну и ту же `&mut` и сохранение не собрать без unsafe.
    pub fs: LocalFs,
    pub dir: String,
}

impl SpoolTransport {
    pub fn new(dir: String) -> Self {
        Self { fs: LocalFs::new(), dir }
    }
}

impl MailTransport for SpoolTransport {
    fn deliver(&mut self, raw: &[u8], suggested_name: &str) -> io::Result<String> {
        self.fs.create_dir_all(&self.dir)?;
        let path = format!("{}\\{}", self.dir.trim_end_matches(['\\', '/']), suggested_name);
        self.fs.write_atomic(&path, raw)?;
        Ok(path)
    }
}

// ---------------------------------------------------------------- цепочка версий

/// Append-only индекс версий: одна строка TSV на сохранение, последняя строка
/// задаёт голову цепочки. Формат намеренно текстовый — его читает и человек,
/// и `verify`, и внешний аудит через `findstr`.
pub struct ChainStore {
    pub fs: LocalFs,
    pub index_path: String,
    head: Option<[u8; 32]>,
    count: u64,
}

impl ChainStore {
    pub fn open(index_path: String) -> io::Result<Self> {
        let mut fs = LocalFs::new();
        let mut head = None;
        let mut count = 0u64;
        if fs.exists(&index_path) {
            let data = fs.read(&index_path)?;
            let text = String::from_utf8_lossy(&data);
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let cols: Vec<&str> = line.split('\t').collect();
                if cols.len() >= 8 {
                    if let Ok(h) = hex32(cols[7]) {
                        head = Some(h);
                        count += 1;
                    }
                }
            }
        }
        Ok(Self { fs, index_path, head, count })
    }
}

fn hex32(s: &str) -> Result<[u8; 32], ()> {
    let b = s.trim().as_bytes();
    if b.len() != 64 {
        return Err(());
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = (hexval(b[i * 2])? << 4) | hexval(b[i * 2 + 1])?;
    }
    Ok(out)
}

fn hexval(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

impl VersionStore for ChainStore {
    fn append(&mut self, rec: &VersionRecord) -> io::Result<()> {
        let mut line = String::with_capacity(320);
        line.push_str(&rec.seq.to_string());
        line.push('\t');
        line.push_str(&rec.unix_secs.to_string());
        line.push('\t');
        line.push_str(&rec.working_file);
        line.push('\t');
        line.push_str(&rec.version_file);
        let mut hx = String::new();
        crate::sha256::hex(&rec.new_hash, &mut hx);
        line.push('\t');
        line.push_str(&hx);
        crate::sha256::hex(&rec.prev_hash, &mut hx);
        line.push('\t');
        line.push_str(&hx);
        line.push('\t');
        line.push_str(&rec.wires);
        line.push('\t');
        crate::sha256::hex(&rec.chain_hash, &mut hx);
        line.push_str(&hx);
        line.push('\n');

        let mut data = if self.fs.exists(&self.index_path) {
            self.fs.read(&self.index_path)?
        } else {
            b"# dxfsign version chain v1: seq\tts\tworking\tversion\tnew_sha256\tprev_sha256\twires\tchain_sha256\n".to_vec()
        };
        data.extend_from_slice(line.as_bytes());
        self.fs_write_index(&data)?;
        self.head = Some(rec.chain_hash);
        self.count += 1;
        Ok(())
    }

    fn head(&self) -> Option<[u8; 32]> {
        self.head
    }

    fn count(&self) -> u64 {
        self.count
    }
}

impl ChainStore {
    fn fs_write_index(&mut self, data: &[u8]) -> io::Result<()> {
        let p = self.index_path.clone();
        self.fs.write_atomic(&p, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_fs_atomic_write_and_list() {
        let dir = std::env::temp_dir().join(format!("dxfsign-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut fs = LocalFs::new();
        let p = dir.join("a.dxf");
        let ps = p.to_string_lossy().to_string();
        fs.write_atomic(&ps, b"hello").unwrap();
        assert_eq!(fs.read(&ps).unwrap(), b"hello");
        fs.write_atomic(&ps, b"hello2").unwrap();
        assert_eq!(fs.read(&ps).unwrap(), b"hello2");
        let list = fs.list(&dir.to_string_lossy()).unwrap();
        assert!(list.iter().any(|e| e.name == "a.dxf"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn signer_without_key_returns_none_not_zero_mac() {
        let s = HmacSigner::unavailable("no-key".into());
        assert!(s.sign(b"x").is_none());
        let s = HmacSigner::new(b"k".to_vec(), "k1".into());
        assert!(s.sign(b"x").is_some());
    }
}
