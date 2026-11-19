//! eml.rs — письмо как артефакт сохранения: RFC 5322 + MIME multipart/mixed.
//!
//! Письмо — не уведомление «кто-то что-то сохранил», а носитель записи:
//!  * вложение 1: новый `.dxf` (то, что теперь лежит на сетевом диске);
//!  * вложение 2: прежний `.dxf` (ровно те байты, что уехали в подпапку версий);
//!  * вложение 3: `.txt` с перечнем правок, включая номера проводов;
//!  * заголовки `X-DXFSIGN-*`: хеши обоих файлов, номер версии, номера проводов;
//!  * `X-DXFSIGN-Signature`: HMAC-SHA256 над канонической строкой (см. `canonical`).
//!
//! Что подпись покрывает и что нет — прямо: HMAC считается по канонической строке
//! (хеши обоих файлов + метаданные). Тело письма base64-закодировано, но его
//! содержимое однозначно задано хешами в подписанной части, поэтому подмена
//! вложения ловится пересчётом SHA-256. Не покрыто: заголовки `Received`,
//! добавляемые почтовым сервером, и сам адрес отправителя — для этого нужна
//! DKIM/SPF на стороне почты, а не утилита.
//!
//! Транспорт: спул-каталог (см. `ports::SpoolTransport`). Отправка по SMTP
//! сознательно не реализована: на промышленной сети это отдельный сервис с
//! согласованным релеем, и делать вид, что утилита умеет SMTP-аутентификацию,
//! было бы обманом.

/// Вложение письма.
pub struct Attachment<'a> {
    pub filename: String,
    /// MIME-тип: `text/plain`, `application/dxf` (не зарегистрирован -> octet-stream),
    pub mime: &'static str,
    pub data: &'a [u8],
}

pub struct EmlMeta<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub subject: String,
    pub date_rfc5322: String,
    pub message_id: String,
    pub body_text: String,
    /// Пары (имя, значение) для `X-DXFSIGN-*` — порядок сохраняется как задан.
    pub x_headers: Vec<(String, String)>,
}

pub const CTE_LINE: usize = 76;

/// Каноническая строка для подписи: ровно те поля, которые проверяющий может
/// пересчитать, не разбирая MIME. Порядок и разделители фиксированы.
pub fn canonical(
    working_file: &str,
    new_hex: &str,
    prev_hex: &str,
    prev_version: &str,
    wires: &str,
    edits: u32,
    unix_ts: i64,
) -> String {
    let mut s = String::with_capacity(256);
    s.push_str("dxfsign-sign/v1\n");
    s.push_str("working=");
    s.push_str(working_file);
    s.push('\n');
    s.push_str("new_sha256=");
    s.push_str(new_hex);
    s.push('\n');
    s.push_str("prev_sha256=");
    s.push_str(prev_hex);
    s.push('\n');
    s.push_str("prev_version=");
    s.push_str(prev_version);
    s.push('\n');
    s.push_str("wires=");
    s.push_str(wires);
    s.push('\n');
    s.push_str("edits=");
    s.push_str(&edits.to_string());
    s.push('\n');
    s.push_str("ts=");
    s.push_str(&unix_ts.to_string());
    s.push('\n');
    s
}

// RFC 2231: не-ASCII имя вложения передаётся как filename*=UTF-8''<percent-encoded>.
fn percent_encode(s: &str, out: &mut String) {
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
}

fn is_ascii(s: &str) -> bool {
    s.is_ascii()
}

/// Кодирование заголовка по RFC 2047: кириллица в Subject и в X-заголовках
/// обязана уехать как `=?UTF-8?B?...?=`. Без этого письмо формально валидно,
/// но тема читается как «���» — это был реальный дефект, пойманый парсером Python.
/// Длинные значения режутся на слова ≤ 45 байт UTF-8 (после base64 — ≤ 60 симв.).
fn enc_header(s: &str) -> String {
    if s.is_ascii() && !s.contains('\n') && !s.contains('\r') {
        return s.to_string();
    }
    let clean: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    let mut out = String::with_capacity(clean.len() * 2);
    let mut chunk = String::new();
    let mut chunk_bytes = 0usize;
    let flush = |out: &mut String, chunk: &mut String| {
        if chunk.is_empty() {
            return;
        }
        let mut b64 = String::new();
        crate::sha256::base64_encode(chunk.as_bytes(), 0, &mut b64);
        if !out.is_empty() {
            out.push('\r');
            out.push('\n');
            out.push(' ');
        }
        out.push_str("=?UTF-8?B?");
        out.push_str(&b64);
        out.push_str("?=");
        chunk.clear();
    };
    for ch in clean.chars() {
        let n = ch.len_utf8();
        if chunk_bytes + n > 45 {
            flush(&mut out, &mut chunk);
            chunk_bytes = 0;
        }
        chunk.push(ch);
        chunk_bytes += n;
    }
    flush(&mut out, &mut chunk);
    out
}

/// Собрать письмо. O(размер вложений). `out` переиспользуется.
pub fn build(meta: &EmlMeta<'_>, atts: &[Attachment<'_>], boundary_seed: u64, out: &mut Vec<u8>) {
    out.clear();
    let boundary = format!("----=_dxfsign_{:016x}", boundary_seed);
    let mut w = |s: &str, out: &mut Vec<u8>| {
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\r\n");
    };
    w(&format!("From: {}", meta.from), out);
    w(&format!("To: {}", meta.to), out);
    w(&format!("Subject: {}", enc_header(&meta.subject)), out);
    w(&format!("Date: {}", meta.date_rfc5322), out);
    w(&format!("Message-ID: {}", meta.message_id), out);
    w("MIME-Version: 1.0", out);
    w("X-DXFSIGN-Version: 1", out);
    for (k, v) in &meta.x_headers {
        if v.is_empty() {
            continue;
        }
        w(&format!("{}: {}", k, enc_header(v)), out);
    }
    w(
        &format!("Content-Type: multipart/mixed; boundary=\"{}\"", boundary),
        out,
    );
    w("", out);
    w("Это письмо создано утилитой dxfsign при сохранении изменений чертежа.", out);
    w("", out);

    // тело
    w(&format!("--{}", boundary), out);
    w("Content-Type: text/plain; charset=utf-8", out);
    w("Content-Transfer-Encoding: 8bit", out);
    w("", out);
    w(&meta.body_text, out);
    w("", out);

    let mut b64 = String::with_capacity(8192);
    for a in atts {
        w(&format!("--{}", boundary), out);
        if is_ascii(&a.filename) {
            w(
                &format!("Content-Type: {}; name=\"{}\"", a.mime, a.filename),
                out,
            );
            w(
                &format!("Content-Disposition: attachment; filename=\"{}\"", a.filename),
                out,
            );
        } else {
            w(&format!("Content-Type: {}", a.mime), out);
            let mut enc = String::new();
            percent_encode(&a.filename, &mut enc);
            w(
                &format!("Content-Disposition: attachment; filename*=UTF-8''{}", enc),
                out,
            );
        }
        w("Content-Transfer-Encoding: base64", out);
        w("", out);
        crate::sha256::base64_encode(a.data, CTE_LINE, &mut b64);
        out.extend_from_slice(b64.as_bytes());
        w("", out);
    }
    w(&format!("--{}--", boundary), out);
}

/// Номер письма: `<ts.seq@dxfsign>` — домен намеренно фиктивный локальный,
/// чтобы письмо не считалось подделкой чужого домена при локальной доставке.
pub fn message_id(unix_ts: i64, seq: u64) -> String {
    format!("<{}.{}@dxfsign.local>", unix_ts, seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_is_stable_and_ordered() {
        let a = canonical("Z:\\p\\a.dxf", "aa", "bb", "_VERSIONS\\a__1.dxf", "W-1,W-2", 2, 1000);
        let b = canonical("Z:\\p\\a.dxf", "aa", "bb", "_VERSIONS\\a__1.dxf", "W-1,W-2", 2, 1000);
        assert_eq!(a, b);
        assert!(a.starts_with("dxfsign-sign/v1\nworking="));
        let c = canonical("Z:\\p\\a.dxf", "aa", "bb", "_VERSIONS\\a__1.dxf", "W-1,W-3", 2, 1000);
        assert_ne!(a, c);
    }

    #[test]
    fn non_ascii_header_is_rfc2047_encoded() {
        let long = "Сохрание: проверка очень длинного значения заголовка с кириллицей и пробелами";
        let enc = enc_header(long);
        assert!(enc.starts_with("=?UTF-8?B?"), "{}", enc);
        assert!(enc.ends_with("?="));
        // каждая encoded-word не длиннее 75 символов вместе с обёрткой
        for word in enc.split("\r\n ") {
            assert!(word.len() <= 75, "слово длинное: {} ({})", word, word.len());
        }
        // ASCII проходит без изменений
        assert_eq!(enc_header("plain ascii"), "plain ascii");
    }

    #[test]
    fn message_has_expected_structure() {
        let meta = EmlMeta {
            from: "dxfsign@localhost",
            to: "[REDACTED]",
            subject: "[DXFSIGN] a.dxf — 1 правок — W-101".to_string(),
            date_rfc5322: "Wed, 19 Nov 2026 11:35:00 +0300".to_string(),
            message_id: message_id(1000, 1),
            body_text: "1 правка".to_string(),
            x_headers: vec![("X-DXFSIGN-New-SHA256".to_string(), "abc".to_string())],
        };
        let txt = "TEXT: a -> b (провод W-101)".as_bytes();
        let dxf = b"0\nEOF\n";
        let atts = vec![
            Attachment { filename: "a.dxf".into(), mime: "application/octet-stream", data: dxf },
            Attachment { filename: "Правки.txt".into(), mime: "text/plain", data: txt },
        ];
        let mut out = Vec::new();
        build(&meta, &atts, 0xdead_beef, &mut out);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("MIME-Version: 1.0"));
        assert!(s.contains("multipart/mixed"));
        assert!(s.contains("X-DXFSIGN-New-SHA256: abc"));
        assert!(s.contains("filename*=UTF-8''"));
        assert!(s.contains("Content-Transfer-Encoding: base64"));
        assert!(s.trim_end().ends_with("--"));
        // CRLF, а не LF: письмо должно пережить любой почтовый шлюз
        assert!(s.contains("\r\n"));
    }
}
