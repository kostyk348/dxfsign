//! config.rs — конфигурация утилиты и каталог сетевого диска.
//!
//! ТЗ: «навигация по папкам сетевого диска — по каталогу, который ведётся вручную
//! как файл конфигурации». Формат файла в ТЗ не задан. Принят INI (расширение
//! `.ini`): его правит человек в блокноте, он не требует зависимостей и парсится
//! 120 строками; альтернативы (TOML/JSON) дают строгость, которой здесь нечего
//! проверять, и требуют библиотеку. Решение вынесено владельцу в отчёте.
//!
//! Структура:
//! ```ini
//! [app]
//! to_addr      = [REDACTED]      ; куда подписывать сохранения
//! from_addr    = dxfsign@localhost
//! spool_dir    = \\srv\scan\dxf\_SPOOL     ; спул писем (сетевой, чтобы уходило с сервера)
//! versions_dir = _VERSIONS                ; имя подпапки версий рядом с чертежом
//! tz_offset    = 180                      ; минуты, MSK = 180
//! keep_versions= 0                        ; 0 = хранить всё
//! key_env      = DXFSIGN_KEY              ; ключ подписи берётся из окружения
//! require_wire = true
//!
//! [catalog]
//! ИТП-3   = \\srv\projects\itp-3\dxf
//! Щиты    = \\srv\projects\shields
//! ```
//!
//! Ключ подписи НЕ хранится в этом файле в открытом виде: путь к файлу ключа
//! (`key_file`) или имя переменной окружения (`key_env`). Значение ключа
//! никогда не печатается и не попадает в письмо — только `key_id`.

#[derive(Clone, Debug, Default)]
pub struct CatalogEntry {
    pub name: String,
    /// Путь на сетевом диске: UNC (`\\srv\share\...`) или буква диска.
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub from_addr: String,
    pub to_addr: String,
    pub spool_dir: String,
    pub versions_dir: String,
    pub tz_offset_min: i32,
    pub keep_versions: usize,
    pub key_env: String,
    pub key_file: String,
    pub key_id: String,
    pub require_wire: bool,
    pub catalog: Vec<CatalogEntry>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            from_addr: "dxfsign@localhost".to_string(),
            to_addr: String::new(),
            spool_dir: "_SPOOL".to_string(),
            versions_dir: "_VERSIONS".to_string(),
            tz_offset_min: 180,
            keep_versions: 0,
            key_env: "DXFSIGN_KEY".to_string(),
            key_file: String::new(),
            key_id: "default".to_string(),
            require_wire: true,
            catalog: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Ключ подписи: файл, иначе переменная окружения. Пусто — подписи нет
    /// (письмо уйдёт с `X-DXFSIGN-Unsigned`, а не с пустым HMAC).
    pub fn load_key(&self) -> Vec<u8> {
        use crate::ports::FileSource;
        if !self.key_file.is_empty() {
            let fs = crate::ports::LocalFs::new();
            if let Ok(b) = fs.read(&self.key_file) {
                return trim_bytes(&b);
            }
        }
        if let Ok(v) = std::env::var(&self.key_env) {
            return v.into_bytes();
        }
        Vec::new()
    }

    /// Каталог по имени из конфига. O(n) по числу записей каталога (их десятки).
    pub fn resolve(&self, name: &str) -> Option<&CatalogEntry> {
        self.catalog.iter().find(|e| e.name.to_lowercase() == name.to_lowercase())
    }
}

fn trim_bytes(b: &[u8]) -> Vec<u8> {
    let mut a = 0;
    let mut e = b.len();
    while a < e && (b[a] == b' ' || b[a] == b'\r' || b[a] == b'\n' || b[a] == b'\t') {
        a += 1;
    }
    while e > a && (b[e - 1] == b' ' || b[e - 1] == b'\r' || b[e - 1] == b'\n' || b[e - 1] == b'\t') {
        e -= 1;
    }
    b[a..e].to_vec()
}

/// Разбор INI. Стоимость O(размер файла); файл читается один раз при старте,
/// не в кадре. Неизвестные ключи не считаются ошибкой, но возвращаются в
/// `warnings`, чтобы опечатка в конфиге не осталась незамеченной.
pub fn parse(text: &str) -> (AppConfig, Vec<String>) {
    let mut cfg = AppConfig::default();
    let mut warnings = Vec::new();
    let mut section = String::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            warnings.push(format!("строка {}: нет '=' — пропущено", lineno + 1));
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim().to_string();
        if section == "catalog" {
            if key.is_empty() {
                continue;
            }
            cfg.catalog.push(CatalogEntry { name: k.trim().to_string(), path: val });
            continue;
        }
        match key.as_str() {
            "to_addr" => cfg.to_addr = val,
            "from_addr" => cfg.from_addr = val,
            "spool_dir" => cfg.spool_dir = val,
            "versions_dir" => cfg.versions_dir = val,
            "tz_offset" => cfg.tz_offset_min = val.parse().unwrap_or(180),
            "keep_versions" => cfg.keep_versions = val.parse().unwrap_or(0),
            "key_env" => cfg.key_env = val,
            "key_file" => cfg.key_file = val,
            "key_id" => cfg.key_id = val,
            "require_wire" => cfg.require_wire = !matches!(val.to_ascii_lowercase().as_str(), "0" | "false" | "no"),
            other => warnings.push(format!("строка {}: неизвестный ключ '{}'", lineno + 1, other)),
        }
    }
    (cfg, warnings)
}

fn strip_comment(line: &str) -> &str {
    // ';' и '#' начинают комментарий, но только если стоят в начале строки или
    // после пробела — иначе путь с '#' в имени файла был бы испорчен.
    let b = line.as_bytes();
    let mut prev = b' ';
    for (i, &c) in b.iter().enumerate() {
        if (c == b';' || c == b'#') && (prev == b' ' || prev == b'\t') {
            return &line[..i];
        }
        prev = c;
    }
    line
}

/// Сериализация конфига обратно в INI (для редактора каталога в окне).
/// Порядок ключей фиксирован, чтобы файл не «дёргался» в git при каждой правке.
pub fn to_ini(cfg: &AppConfig) -> String {
    let mut s = String::with_capacity(512 + cfg.catalog.len() * 64);
    s.push_str("; dxfsign — конфигурация\n[app]\n");
    s.push_str(&format!("to_addr      = {}\n", cfg.to_addr));
    s.push_str(&format!("from_addr    = {}\n", cfg.from_addr));
    s.push_str(&format!("spool_dir    = {}\n", cfg.spool_dir));
    s.push_str(&format!("versions_dir = {}\n", cfg.versions_dir));
    s.push_str(&format!("tz_offset    = {}\n", cfg.tz_offset_min));
    s.push_str(&format!("keep_versions= {}\n", cfg.keep_versions));
    s.push_str(&format!("key_env      = {}\n", cfg.key_env));
    if !cfg.key_file.is_empty() {
        s.push_str(&format!("key_file     = {}\n", cfg.key_file));
    }
    s.push_str(&format!("key_id       = {}\n", cfg.key_id));
    s.push_str(&format!("require_wire = {}\n", cfg.require_wire));
    s.push_str("\n[catalog]\n");
    for e in &cfg.catalog {
        s.push_str(&format!("{} = {}\n", e.name, e.path));
    }
    s
}

/// Записать конфиг на диск. O(размер файла).
pub fn save_to(path: &str, cfg: &AppConfig) -> std::io::Result<()> {
    std::fs::write(path, to_ini(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
; конфиг цеха
[app]
to_addr = [REDACTED]
tz_offset = 180
keep_versions = 20
require_wire = true
key_env = DXFSIGN_KEY

[catalog]
ИТП-3 = \\srv\projects\itp-3\dxf
Щиты = \\srv\projects\shields
"#;

    #[test]
    fn parses_app_and_catalog() {
        let (cfg, warn) = parse(SAMPLE);
        assert!(warn.is_empty(), "{:?}", warn);
        assert_eq!(cfg.to_addr, "[REDACTED]");
        assert_eq!(cfg.tz_offset_min, 180);
        assert_eq!(cfg.keep_versions, 20);
        assert!(cfg.require_wire);
        assert_eq!(cfg.catalog.len(), 2);
        assert_eq!(cfg.resolve("итп-3").unwrap().path, r"\\srv\projects\itp-3\dxf");
        assert_eq!(cfg.resolve("Щиты").unwrap().path, r"\\srv\projects\shields");
    }

    #[test]
    fn unknown_key_is_reported_not_ignored_silently() {
        let (_, warn) = parse("[app]\nto_adr = x\n");
        assert_eq!(warn.len(), 1);
        assert!(warn[0].contains("to_adr"));
    }

    #[test]
    fn ini_roundtrip_keeps_catalog() {
        let (cfg, _) = parse(SAMPLE);
        let text = to_ini(&cfg);
        let (cfg2, warn) = parse(&text);
        assert!(warn.is_empty(), "{:?}", warn);
        assert_eq!(cfg2.catalog.len(), cfg.catalog.len());
        assert_eq!(cfg2.resolve("ИТП-3").unwrap().path, cfg.resolve("ИТП-3").unwrap().path);
        assert_eq!(cfg2.tz_offset_min, cfg.tz_offset_min);
        assert_eq!(cfg2.keep_versions, cfg.keep_versions);
    }

    #[test]
    fn hash_inside_path_is_not_a_comment() {
        let line = r"path = \\srv\a#b\c";
        assert_eq!(strip_comment(line), line);
        let line2 = "path = x ; пояснение";
        assert_eq!(strip_comment(line2), "path = x ");
    }
}
