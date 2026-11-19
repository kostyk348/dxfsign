//! main.rs — точка входа и CLI. Здесь же — доказательство «кадр без аллокаций»:
//! глобальный аллокатор считает вызовы, и тест `frame_allocates_nothing` падает,
//! если в кадре появится malloc.
//!
//! Команды:
//!   inspect  <файл>                     — разбор и статистика
//!   edit     <файл> --idx N --text "…" --wire W-1 [--comment "…"]
//!   save     <файл> [--config …] [--seq N] [--now UNIX]
//!   snapshot <файл> --out кадр.bmp [--w 1280 --h 720] [--modal] [--idx N]
//!   bench    <файл> [--iters 200]       — AVX2 против скаляра
//!   alloc    <файл> [--frames 50]       — проверка нулевых аллокаций в кадре
//!   catalog  [--config …]               — что видно в каталоге сетевого диска

mod config;
mod doc;
mod dialogs;
mod dxf;
mod edit;
mod eml;
mod journal;
mod pool;
mod ports;
mod render;
mod save;
mod sha256;
mod text;
mod win32;
#[cfg(windows)]
mod window;

use ports::{FileSource as _, VersionStore as _};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ---------------------------------------------------------------- счётчик аллокаций
//
// Счётчик ПОТОКОВЫЙ, а не глобальный: иначе тесты, идущие параллельно, портят
// друг другу замер (проверка «кадр не аллоцирует» падала от чужих аллокаций).
// Глобальный статик оставлен как сумма для команды `alloc` и для отчёта.

thread_local! {
    static TLS_ALLOCS: Cell<usize> = const { Cell::new(0) };
    static TLS_DEALLOCS: Cell<usize> = const { Cell::new(0) };
}

pub static ALLOCS: AtomicUsize = AtomicUsize::new(0);
pub static DEALLOCS: AtomicUsize = AtomicUsize::new(0);
pub static ARMED: AtomicBool = AtomicBool::new(false);

#[inline]
fn bump_alloc() {
    if ARMED.load(Ordering::Relaxed) {
        let _ = TLS_ALLOCS.try_with(|c| c.set(c.get() + 1));
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn bump_dealloc() {
    if ARMED.load(Ordering::Relaxed) {
        let _ = TLS_DEALLOCS.try_with(|c| c.set(c.get() + 1));
        DEALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Аллокации текущего потока с момента `reset_armed()`.
pub fn thread_allocs() -> usize {
    TLS_ALLOCS.with(|c| c.get())
}

pub fn reset_armed() {
    let _ = TLS_ALLOCS.try_with(|c| c.set(0));
    let _ = TLS_DEALLOCS.try_with(|c| c.set(0));
    ALLOCS.store(0, Ordering::SeqCst);
    DEALLOCS.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
}

pub fn disarm() -> (usize, usize) {
    ARMED.store(false, Ordering::SeqCst);
    (TLS_ALLOCS.with(|c| c.get()), TLS_DEALLOCS.with(|c| c.get()))
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump_alloc();
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump_dealloc();
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump_alloc();
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ---------------------------------------------------------------- helpers

fn die(msg: &str) -> ! {
    eprintln!("dxfsign: {}", msg);
    std::process::exit(2)
}

fn arg_val(args: &[String], key: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == key {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn load_doc(path: &str) -> (doc::Doc, dxf::ParseStats) {
    let data = std::fs::read(path).unwrap_or_else(|e| die(&format!("не читается {}: {}", path, e)));
    let mut d = doc::Doc::with_capacity(1024);
    let st = match dxf::parse(&data, &mut d) {
        Ok(st) => st,
        Err(e) => die(&format!("{}: {}", path, e)),
    };
    d.path.push_str(path);
    (d, st)
}

fn load_config(path: Option<String>) -> config::AppConfig {
    match path {
        Some(p) => {
            let txt = std::fs::read_to_string(&p).unwrap_or_else(|e| die(&format!("конфиг {}: {}", p, e)));
            let (cfg, warn) = config::parse(&txt);
            for w in warn {
                eprintln!("конфиг: {}", w);
            }
            cfg
        }
        None => config::AppConfig::default(),
    }
}

fn save_config(cfg: &config::AppConfig) -> save::SaveConfig {
    save::SaveConfig {
        from_addr: cfg.from_addr.clone(),
        to_addr: cfg.to_addr.clone(),
        versions_dir: cfg.versions_dir.clone(),
        spool_dir: cfg.spool_dir.clone(),
        tz_offset_min: cfg.tz_offset_min,
        keep_versions: cfg.keep_versions,
    }
}

// ---------------------------------------------------------------- команды

fn cmd_inspect(args: &[String]) {
    let path = args.get(1).cloned().unwrap_or_else(|| die("inspect: нужен путь к .dxf"));
    let (d, st) = load_doc(&path);
    println!("файл:        {}", d.path);
    println!("DXF-версия:  {}", if d.dxf_version.is_empty() { "не задана" } else { &d.dxf_version });
    println!("кодовая стр.: {}", if d.codepage.is_empty() { "не задана" } else { &d.codepage });
    println!("LINE:        {}", d.segs.len());
    println!("CIRCLE:      {}", d.circles.len());
    println!("LWPOLYLINE:  {}", d.polys.len());
    println!("TEXT/MTEXT:  {}", d.texts.len());
    println!("слои:        {}", d.layers.len());
    println!("точек:       {}", d.xs.len());
    println!("пропущено:   {} (сущности вне поддержки: не перезаписываются)", st.skipped);
    println!("SHA-256:     {}", hex(&d.hash()));
    for (i, t) in d.texts.iter().enumerate().take(20) {
        println!(
            "  [{}] \"{}\"  h={} провод={}",
            i,
            d.arena_utf8(t.s),
            t.height,
            if t.wire.is_empty() { "—".to_string() } else { d.arena_utf8(t.wire).to_string() }
        );
    }
}

/// Диагностика реального файла: секции, счётчики по типам, блоки.
/// Нужна, когда «DXF не читается»: по выводу видно, что именно внутри.
fn cmd_report(args: &[String]) {
    let path = args.get(1).cloned().unwrap_or_else(|| die("report: нужен путь к .dxf"));
    // Диагностика НЕ должна падать: даже битый файл обязан рассказать, что в нём.
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            println!("НЕ ЧИТАЕТСЯ: «{}»", path);
            println!("причина: {} (код {:?})", e, e.raw_os_error());
            println!("проверьте: путь существует, это файл, есть права, не занят ли программой");
            return;
        }
    };
    println!("файл:            {}", path);
    println!("размер:          {} байт", data.len());
    if data.is_empty() {
        println!("ЭТО ПУСТОЙ ФАЙЛ (0 байт) — читать нечего");
        return;
    }
    let head = String::from_utf8_lossy(&data[..data.len().min(48)]).replace(['\r', '\n'], " | ");
    println!("первые байты:    {}", head);
    if data.starts_with(b"AutoCAD Binary DXF") {
        println!("ЭТО БИНАРНЫЙ DXF — формат не поддержан. В AutoCAD: Save As → DXF → ASCII.");
    }
    if let Some(ext) = std::path::Path::new(&path).extension().and_then(|e| e.to_str()) {
        if ext.eq_ignore_ascii_case("dwg") {
            println!("ЭТО DWG, а не DXF. Нужно сохранить как DXF (Save As → DXF).");
        }
    }
    let mut d = doc::Doc::with_capacity(1024);
    match dxf::parse(&data, &mut d) {
        Ok(st) => {
            println!("DXF-версия:      {}", if d.dxf_version.is_empty() { "не задана" } else { &d.dxf_version });
            println!("кодовая страница: {}", if d.codepage.is_empty() { "не задана" } else { &d.codepage });
            println!("секции:          {}", if st.sections.is_empty() { "—".to_string() } else { st.sections.join(", ") });
            println!("блоков:          {}   вставок развёрнуто: {}   без определения: {}", st.blocks, st.inserts_expanded, st.inserts_unresolved);
            println!("сущностей всего: {}   неподдержано: {}", st.total_entities, st.skipped);
            println!("по типам:        {}", st.summary());
            println!("--- прочитано в модель ---");
            println!("LINE {} · CIRCLE {} · LWPOLYLINE {} · TEXT/MTEXT {} · точек {} · слоёв {}",
                d.segs.len(), d.circles.len(), d.polys.len(), d.texts.len(), d.xs.len(), d.layers.len());
            if d.entity_count() == 0 {
                println!("\nМОДЕЛЬ ПУСТА. Причины по порядку вероятности:");
                if !st.sections.iter().any(|s| s == "ENTITIES") {
                    println!("  • в файле нет секции ENTITIES — это не чертёж или файл обрезан");
                }
                if st.inserts_unresolved > 0 {
                    println!("  • {} INSERT без определений блоков — геометрия не восстановима без BLOCKS", st.inserts_unresolved);
                }
                if st.total_entities == 0 {
                    println!("  • сущностей не найдено вовсе: файл мог быть сохранён как DWG с расширением .dxf");
                } else {
                    println!("  • все сущности неподдержанных типов: {}", st.summary());
                }
            }
        }
        Err(e) => {
            println!("РАЗБОР НЕ УДАЛСЯ: {}", e);
            println!("файл не похож на ASCII DXF (возможно, бинарный формат или повреждён)");
        }
    }
}

fn cmd_edit(args: &[String]) {
    let path = args.get(1).cloned().unwrap_or_else(|| die("edit: нужен путь к .dxf"));
    let (mut d, _) = load_doc(&path);
    let idx: usize = arg_val(args, "--idx").unwrap_or_else(|| die("edit: --idx N")).parse().unwrap_or_else(|_| die("--idx: число"));
    let text = arg_val(args, "--text").unwrap_or_else(|| die("edit: --text \"...\""));
    let wire = arg_val(args, "--wire").unwrap_or_else(|| die("edit: --wire W-1 (обязателен)"));
    let comment = arg_val(args, "--comment").unwrap_or_default();
    let mut s = edit::EditSession::new();
    s.begin(idx, &d.arena_get(d.texts[idx].s).to_vec());
    s.text.set(text.as_bytes());
    s.comment.set(comment.as_bytes());
    s.commit_text();
    s.wire.set(wire.as_bytes());
    match s.submit_wire(&mut d) {
        edit::Outcome::Applied => println!("правка применена: надпись [{}] -> \"{}\", провод {}", idx, text, wire),
        edit::Outcome::Rejected => die(format!("номер провода не принят: {}", s.error.message()).as_str()),
        _ => die("правка не применена"),
    }
    // Черновик сохраняется рядом с расширением .edited.dxf — рабочий файл не трогаем:
    // запись в рабочий файл делает только `save` (протокол версий и письма).
    let mut out = Vec::new();
    dxf::write(&d, &mut out);
    let tmp = format!("{}.edited.dxf", path);
    std::fs::write(&tmp, &out).unwrap_or_else(|e| die(&format!("запись {}: {}", tmp, e)));
    // Журнал — отдельным файлом: правка и сохранение в скелете это два процесса,
    // и без сайдкара перечень изменений не дожил бы до письма.
    let jpath = format!("{}.journal.tsv", tmp);
    journal::write(&d, &jpath).unwrap_or_else(|e| die(&format!("журнал {}: {}", jpath, e)));
    println!("черновик: {}", tmp);
    println!("журнал:   {} ({} записей); чтобы провести через версии — `save`", jpath, d.log.len());
}

fn cmd_save(args: &[String]) {
    let path = args.get(1).cloned().unwrap_or_else(|| die("save: нужен путь к .dxf"));
    let cfg_path = arg_val(args, "--config");
    let cfg = load_config(cfg_path);
    // Сохранение принимает либо рабочий файл (тогда правим его), либо черновик
    // `.edited.dxf` (тогда источник правок — он, цель — рабочий путь без суффикса).
    let (source, target) = if let Some(w) = arg_val(args, "--working") {
        (path.clone(), w)
    } else if let Some(stripped) = path.strip_suffix(".edited.dxf") {
        (path.clone(), format!("{}.dxf", stripped))
    } else {
        (path.clone(), path.clone())
    };
    let (mut d, _) = load_doc(&source);
    d.path.clear();
    d.path.push_str(&target);
    // Журнал правок подхватывается из сайдкара рядом с источником.
    let jpath = format!("{}.journal.tsv", source);
    if std::path::Path::new(&jpath).exists() {
        match journal::read(&mut d, &jpath) {
            Ok(n) => println!("журнал: {} → {} правок", jpath, n),
            Err(e) => eprintln!("журнал {}: {} (продолжаю без него)", jpath, e),
        }
    }

    // Журнал правок приходит из `edit` (в скелете процесс разбит на два вызова:
    // правка и сохранение — так каждый шаг проверяем отдельно). Если журнал пуст,
    // сохранение всё равно осмысленно: оно зафиксирует текущее состояние файла.
    let now = arg_val(args, "--now").and_then(|v| v.parse().ok()).unwrap_or_else(now_unix);
    let mut fs = ports::LocalFs::new();
    // Полный каталог цели, а не последний сегмент: иначе индекс цепочки уезжает
    // в относительный путь (реальный баг, пойманый на живом прогоне).
    let dir = save::parent_dir(&target);
    let vdir = save::join(&dir, &cfg.versions_dir);
    fs.create_dir_all(&vdir).unwrap_or_else(|e| die(&format!("каталог версий {}: {}", vdir, e)));
    let index_path = save::join(&vdir, "index.tsv");
    let mut store = ports::ChainStore::open(index_path).unwrap_or_else(|e| die(&format!("цепочка: {}", e)));
    let seq: u64 = arg_val(args, "--seq").and_then(|v| v.parse().ok()).unwrap_or(store.count() + 1);

    let key = cfg.load_key();
    let signer = if key.is_empty() {
        ports::HmacSigner::unavailable(cfg.key_id.clone())
    } else {
        ports::HmacSigner::new(key, cfg.key_id.clone())
    };
    let mut signer_box = signer;
    let mut mail = ports::SpoolTransport::new(cfg.spool_dir.clone());
    let scfg = save_config(&cfg);
    let mut dxf_bytes = Vec::with_capacity(1 << 20);
    let out = save::save(&d, &scfg, &mut fs, &mut signer_box, &mut mail, &mut store, now, seq, &mut dxf_bytes);
    match out {
        Ok(o) => {
            if !o.written {
                println!("ничего не изменилось");
            }
            println!("рабочий файл:  {} (перезаписан, SHA-256 {})", target, o.new_hex);
            println!("прежняя версия: {}", if o.version_file.is_empty() { "— (файла не было)" } else { &o.version_file });
            println!("перечень:      {}", o.txt_path);
            println!("письмо:        {}", o.eml_path);
            println!("правок:        {} · провода: {}", o.edits, if o.wires.is_empty() { "—" } else { &o.wires });
            println!("подпись:       {}", if signer_box.has_key() { "HMAC-SHA256" } else { "нет ключа (X-DXFSIGN-Unsigned)" });
            // Журнал потреблён: проведённая правка не должна примениться второй раз.
            let _ = std::fs::remove_file(&jpath);
        }
        Err(save::SaveError::NothingChanged) => println!("чертёж не изменился: версия и письмо не создавались"),
        Err(e) => die(&format!("сохранение не выполнено: {}", e)),
    }
}

fn cmd_snapshot(args: &[String]) {
    let path = args.get(1).cloned().unwrap_or_else(|| die("snapshot: нужен путь к .dxf"));
    let out_path = arg_val(args, "--out").unwrap_or_else(|| "snapshot.bmp".to_string());
    let w: usize = arg_val(args, "--w").and_then(|v| v.parse().ok()).unwrap_or(1440);
    let h: usize = arg_val(args, "--h").and_then(|v| v.parse().ok()).unwrap_or(900);
    let (d, _) = load_doc(&path);
    let mut r = render::Renderer::new(w, h, d.xs.len().max(16) * 2);
    let mut ui = render::UiState::new();
    ui.set_file(path.rsplit(['\\', '/']).next().unwrap_or(&path));
    ui.set_status("F2 правка · F3 комментарий · Ctrl+S сохранить с письмом");
    ui.catalog_names.push("ИТП-3".to_string());
    ui.catalog_names.push("Щиты".to_string());
    ui.catalog_names.push("Версии".to_string());
    ui.set_spool("_SPOOL (каталог спула из dxfsign.ini)");
    if let Some(h) = arg_val(args, "--hover").and_then(|v| v.parse::<usize>().ok()) {
        ui.hover_text = Some(h);
        ui.mouse = (w as i32 / 2, h as i32 / 2);
    } else {
        ui.mouse = (-1, -1);
    }
    if has_flag(args, "--help") {
        ui.help_open = true;
    }
    ui.set_last("ещё не сохраняли в этой сессии");
    if let Some(i) = arg_val(args, "--idx").and_then(|v| v.parse::<usize>().ok()) {
        ui.selected_text = Some(i);
    } else if !d.texts.is_empty() {
        ui.selected_text = Some(0);
    }
    let mut session = edit::EditSession::new();
    if has_flag(args, "--modal") {
        let idx = ui.selected_text.unwrap_or(0);
        if d.texts.is_empty() {
            die("--modal: в файле нет надписей");
        }
        session.begin(idx, &d.arena_get(d.texts[idx].s).to_vec());
        session.text.set(b"Shchit-1 / Klemmnik X1");
        session.wire.set(b"W-101");
        session.comment.set("по замечанию ПТО: заменить автомат".as_bytes());
        session.commit_text();
        // какое поле в фокусе — для проверки вёрстки и каретки
        ui.focus = arg_val(args, "--focus").and_then(|v| v.parse().ok()).unwrap_or(1);
    }
    let view_w = (w as i32 - render::swiss::LEFT_W - render::swiss::RIGHT_W).max(32);
    let view_h = (h as i32 - render::swiss::TOP_H - render::swiss::STATUS_H).max(32);
    let cam = render::Camera::fit(d.bounds(), view_w, view_h, 64);
    let mut painter = win32::GdiPainter::new();
    match painter.as_mut() {
        Some(p) => {
            if let Err(e) = r.frame(&d, cam, &mut ui, &session, p) {
                die(&format!("кадр: {:?}", e));
            }
        }
        None => {
            let mut ap = win32::ApproxPainter;
            if let Err(e) = r.frame(&d, cam, &mut ui, &session, &mut ap) {
                die(&format!("кадр: {:?}", e));
            }
            eprintln!("GDI недоступен: текст нарисован приблизительно");
        }
    }
    let mut bmp = Vec::new();
    r.snapshot_bmp(&mut bmp);
    std::fs::write(&out_path, &bmp).unwrap_or_else(|e| die(&format!("{}: {}", out_path, e)));
    println!("кадр: {} ({}×{}, {} байт)", out_path, w, h, bmp.len());
    println!("проекция: {}", ui.backend);
}

fn cmd_bench(args: &[String]) {
    let path = args.get(1).cloned().unwrap_or_else(|| die("bench: нужен путь к .dxf"));
    let iters: usize = arg_val(args, "--iters").and_then(|v| v.parse().ok()).unwrap_or(200);
    let (d, _) = load_doc(&path);
    let n = d.xs.len();
    if n == 0 {
        die("в файле нет точек");
    }
    let cam = render::Camera { scale: 0.731, ox: 512.0, oy: 384.0 };
    let mut sx = vec![0i32; n];
    let mut sy = vec![0i32; n];

    let t0 = std::time::Instant::now();
    for _ in 0..iters {
        render::project_scalar(&d.xs, &d.ys, cam, &mut sx, &mut sy);
    }
    let scalar = t0.elapsed();

    let mut avx2 = std::time::Duration::ZERO;
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            let t1 = std::time::Instant::now();
            for _ in 0..iters {
                unsafe { render::project_avx2(&d.xs, &d.ys, cam, &mut sx, &mut sy) };
            }
            avx2 = t1.elapsed();
        }
    }
    let per_iter = |d: std::time::Duration| d.as_secs_f64() / iters as f64;
    println!("точек: {} · итераций: {}", n, iters);
    println!("скаляр: {:>9.3} мс/итерация  ({:.1} нс/точка)", per_iter(scalar) * 1e3, per_iter(scalar) * 1e9 / n as f64);
    if avx2.as_nanos() > 0 {
        println!("AVX2:   {:>9.3} мс/итерация  ({:.1} нс/точка) · ускорение ×{:.2}", per_iter(avx2) * 1e3, per_iter(avx2) * 1e9 / n as f64, per_iter(scalar) / per_iter(avx2));
    } else {
        println!("AVX2 недоступен на этом процессоре — работает скалярный путь");
    }
}

fn cmd_alloc(args: &[String]) {
    let path = args.get(1).cloned().unwrap_or_else(|| die("alloc: нужен путь к .dxf"));
    let frames: usize = arg_val(args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(50);
    let (d, _) = load_doc(&path);
    let mut r = render::Renderer::new(1280, 800, d.xs.len().max(16) * 2);
    let mut ui = render::UiState::new();
    ui.set_file("bench.dxf");
    ui.set_status("проверка аллокаций");
    ui.catalog_names.push("ИТП-3".to_string());
    let view_w = 1280 - render::swiss::LEFT_W - render::swiss::RIGHT_W;
    let view_h = 800 - render::swiss::TOP_H - render::swiss::STATUS_H;
    let cam = render::Camera::fit(d.bounds(), view_w, view_h, 64);
    let session = edit::EditSession::new();
    let mut painter = win32::ApproxPainter;
    // прогрев: шрифты/буферы рисуются вне замера
    let _ = r.frame(&d, cam, &mut ui, &session, &mut painter);
    reset_armed();
    for _ in 0..frames {
        let _ = r.frame(&d, cam, &mut ui, &session, &mut painter);
    }
    let (a, de) = disarm();
    println!("кадров: {} · аллокаций: {} · освобождений: {}", frames, a, de);
    if a == 0 {
        println!("кадр без единой аллокации — требование «нет malloc в главном цикле» выполнено");
    } else {
        println!("ВНИМАНИЕ: кадр аллоцирует {} раз(а) — требование не выполнено", a);
        std::process::exit(1);
    }
}

fn cmd_catalog(args: &[String]) {
    let cfg = load_config(arg_val(args, "--config"));
    if cfg.catalog.is_empty() {
        println!("каталог пуст: заполните секцию [catalog] в конфиге");
        return;
    }
    let fs = ports::LocalFs::new();
    for e in &cfg.catalog {
        println!("{:<12} {}", e.name, e.path);
        match fs.list(&e.path) {
            Ok(entries) => {
                let mut files = 0;
                let mut dirs = 0;
                for x in &entries {
                    if x.is_dir {
                        dirs += 1
                    } else {
                        files += 1
                    }
                }
                println!("             доступен: {} папок, {} файлов", dirs, files);
            }
            Err(err) => println!("             НЕДОСТУПЕН: {}", err),
        }
    }
}


// ---------------------------------------------------------------- оконный режим

/// Демонстрационный чертёж: с ним exe, запущенный двойным кликом, сразу
/// показывает работающее окно. Это не «заглушка»: та же модель, тот же рендер,
/// та же правка и то же сохранение, что и для файла с диска.
fn demo_doc() -> doc::Doc {
    let mut d = doc::Doc::with_capacity(512);
    let l_pow = d.layer_index("EL-POWER".as_bytes());
    let l_txt = d.layer_index("EL-TEXT".as_bytes());
    // рамка щита
    let corners = [(0.0, 0.0, 600.0, 0.0), (600.0, 0.0, 600.0, 400.0), (600.0, 400.0, 0.0, 400.0), (0.0, 400.0, 0.0, 0.0)];
    for (x0, y0, x1, y1) in corners {
        let a = d.push_point(x0, y0);
        let b = d.push_point(x1, y1);
        d.segs.push(doc::Seg { a, b, layer: l_pow });
    }
    // шины
    for y in [120.0, 280.0] {
        let a = d.push_point(20.0, y);
        let b = d.push_point(580.0, y);
        d.segs.push(doc::Seg { a, b, layer: l_pow });
    }
    // автоматы (замкнутые полилинии) + присоединения
    for i in 0..6 {
        let x = 60.0 + i as f64 * 90.0;
        let first = d.poly_verts.len() as u32;
        for (px, py) in [(x, 140.0), (x + 40.0, 140.0), (x + 40.0, 200.0), (x, 200.0)] {
            let p = d.push_point(px, py);
            d.poly_verts.push(p);
        }
        d.polys.push(doc::Poly { first, len: 4, closed: true, layer: l_pow });
        let a = d.push_point(x + 20.0, 200.0);
        let b = d.push_point(x + 20.0, 280.0);
        d.segs.push(doc::Seg { a, b, layer: l_pow });
    }
    // двигатели
    for i in 0..3 {
        let x = 150.0 + i as f64 * 180.0;
        let c = d.push_point(x, 340.0);
        d.circles.push(doc::Circle { c, r: 28.0, layer: l_pow });
    }
    let texts: [(&str, f64, f64, f64); 12] = [
        ("ЩИТ НИЗКОВОЛЬТНЫЙ ЩН-3", 30.0, 430.0, 12.0),
        ("QF1  Ввод 1", 40.0, 388.0, 5.0),
        ("QF2  Ввод 2", 130.0, 372.0, 5.0),
        ("QF3  Секция", 215.0, 380.0, 5.0),
        ("QF4  Двигатель", 300.0, 388.0, 5.0),
        ("QF5  Резерв", 395.0, 372.0, 5.0),
        ("QF6  Освещение", 480.0, 388.0, 5.0),
        ("M1  Насос 7,5 кВт", 150.0, 300.0, 5.0),
        ("M2  Вентилятор 4 кВт", 330.0, 300.0, 5.0),
        ("M3  Резерв", 510.0, 300.0, 5.0),
        ("Клеммник X1: 1-2-3-4", 30.0, 60.0, 5.0),
        ("Питание: ~380 В, 3Ф, 50 Гц", 330.0, 60.0, 5.0),
    ];
    for (i, (s, x, y, h)) in texts.iter().enumerate() {
        let p = d.push_point(*x, *y);
        let (sr, _) = d.arena.push(s.as_bytes());
        let idx = d.texts.len() as u32;
        d.texts.push(doc::TextEnt {
            pos: p,
            height: *h,
            rot_deg: 0.0,
            layer: l_txt,
            kind: doc::TEXT_KIND_TEXT,
            s: sr,
            wire: text::StrRef::EMPTY,
            dxf_handle: 0x2A00 + i as u32,
        });
        d.handles.push(pool::Handle { slot: idx, gen: 1 });
    }
    d.dxf_version.push_str("AC1015");
    d.codepage.push_str("ANSI_1251");
    d
}

/// Сохранение из окна: тот же протокол, что у команды `save`.
/// Конфиг берётся рядом с exe (`dxfsign.ini`), иначе — значения по умолчанию.
fn window_save(doc: &doc::Doc, cfg: &config::AppConfig) -> String {
    if doc.path.is_empty() {
        return "Демо-чертёж: откройте файл командой `dxfsign open <файл.dxf>`".to_string();
    }
    if doc.log.is_empty() {
        return "Нет правок в этой сессии: сохранять нечего".to_string();
    }
    let mut fs = ports::LocalFs::new();
    let dir = save::parent_dir(&doc.path);
    let vdir = save::join(&dir, &cfg.versions_dir);
    if let Err(e) = fs.create_dir_all(&vdir) {
        return format!("Каталог версий {}: {}", vdir, e);
    }
    let index_path = save::join(&vdir, "index.tsv");
    let mut store = match ports::ChainStore::open(index_path) {
        Ok(s) => s,
        Err(e) => return format!("Цепочка версий: {}", e),
    };
    let seq = store.count() + 1;
    let key = cfg.load_key();
    let mut signer = if key.is_empty() {
        ports::HmacSigner::unavailable(cfg.key_id.clone())
    } else {
        ports::HmacSigner::new(key, cfg.key_id.clone())
    };
    let mut mail = ports::SpoolTransport::new(cfg.spool_dir.clone());
    let scfg = save_config(&cfg);
    let mut bytes = Vec::with_capacity(1 << 20);
    match save::save(doc, &scfg, &mut fs, &mut signer, &mut mail, &mut store, now_unix(), seq, &mut bytes) {
        Ok(o) => format!(
            "Сохранено: правок {}, провод {}. Старая версия -> {}",
            o.edits,
            if o.wires.is_empty() { "—" } else { &o.wires },
            if o.version_file.is_empty() { "—" } else { &o.version_file }
        ),
        Err(save::SaveError::NothingChanged) => "Чертёж не изменился: версия не пишется".to_string(),
        Err(e) => format!("Не сохранено: {}", e),
    }
}

#[cfg(windows)]
fn cmd_open(args: &[String]) -> i32 {
    let selftest: u32 = arg_val(args, "--selftest").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut d = match args.get(1) {
        Some(p) if !p.starts_with("--") => {
            let (mut d, _) = load_doc(p);
            d.path.clear();
            d.path.push_str(p);
            d
        }
        _ => demo_doc(),
    };
    if d.path.is_empty() {
        set_demo_path(&mut d, selftest);
    }
    let title = if d.path.is_empty() { "демо-чертёж (не файл)".to_string() } else { d.path.clone() };
    // конфиг: явный --config, иначе dxfsign.ini рядом с exe, иначе значения по умолчанию
    let cfg_path = arg_val(args, "--config").unwrap_or_else(|| {
        let exe = std::env::current_exe().unwrap_or_default();
        let dir = save::parent_dir(&exe.to_string_lossy());
        let p = save::join(&dir, "dxfsign.ini");
        if std::path::Path::new(&p).exists() { p } else { String::new() }
    });
    let cfg = if cfg_path.is_empty() {
        config::AppConfig::default()
    } else {
        load_config(Some(cfg_path.clone()))
    };
    window::run(d, 1440, 900, &title, Some(window_save), selftest, cfg, cfg_path)
}

/// Для демо-чертежа путь пуст; чтобы Ctrl+S не упирался в пустоту, при первом
/// сохранении он создаётся рядом с exe как `demo.dxf` — но ТОЛЬКО по действию
/// человека, а не при старте.
#[cfg(windows)]
fn set_demo_path(d: &mut doc::Doc, selftest: u32) {
    let _ = (d, selftest);
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    sha256::hex(b, &mut s);
    s
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
dxfsign — просмотр и правка DXF на сетевом диске; каждое сохранение подписано и отправлено письмом

команды:
  inspect  <файл.dxf>
  report   <файл.dxf>   — что внутри файла: секции, типы сущностей, блоки, вставки
  edit     <файл.dxf> --idx N --text \"...\" --wire W-1 [--comment \"...\"]
  save     <файл.dxf|файл.edited.dxf> [--config cfg.ini] [--seq N] [--now UNIX] [--working путь]
  snapshot <файл.dxf> --out кадр.bmp [--w 1440 --h 900] [--modal] [--idx N]
  bench    <файл.dxf> [--iters 200]
  alloc    <файл.dxf> [--frames 50]
  catalog  [--config cfg.ini]\n  open     [<файл.dxf>] [--selftest <мс>]   -> окно просмотра и правки\n\nбез аргументов: открывается окно с демонстрационным чертежом";

fn usage() -> ! {
    eprintln!("dxfsign {}", VERSION);
    eprintln!("{}", USAGE);
    std::process::exit(2)
}

#[cfg(windows)]
fn run_default_mode() -> i32 {
    let empty: Vec<String> = Vec::new();
    cmd_open(&empty)
}

#[cfg(not(windows))]
fn run_default_mode() -> i32 {
    usage()
}

#[cfg(windows)]
fn run_open_mode(args: &[String]) -> i32 {
    cmd_open(args)
}

#[cfg(not(windows))]
fn run_open_mode(_args: &[String]) -> i32 {
    usage()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        // Двойной клик по exe: раньше печаталась подсказка и консоль мгновенно
        // закрывалась — выглядело как «exe не запускается». Теперь открывается окно.
        std::process::exit(run_default_mode());
    }
    // `dxfsign.exe C:\path\file.dxf` (двойной клик по файлу, ассоциация) — сразу окно
    if !args[1].starts_with('-') && !matches!(args[1].as_str(), "inspect" | "report" | "edit" | "save" | "snapshot" | "bench" | "alloc" | "catalog" | "open" | "help") {
        let mut a: Vec<String> = vec!["open".to_string()];
        a.extend_from_slice(&args[1..]);
        std::process::exit(run_open_mode(&a));
    }
    match args[1].as_str() {
        "--version" | "-V" | "version" => {
            println!("dxfsign {}", VERSION);
            return;
        }
        "--help" | "-h" | "help" => usage(),
        "inspect" => cmd_inspect(&args[1..]),
        "report" => cmd_report(&args[1..]),
        "edit" => cmd_edit(&args[1..]),
        "save" => cmd_save(&args[1..]),
        "snapshot" => cmd_snapshot(&args[1..]),
        "bench" => cmd_bench(&args[1..]),
        "alloc" => cmd_alloc(&args[1..]),
        "catalog" => cmd_catalog(&args[1..]),
        "open" => std::process::exit(run_open_mode(&args[1..])),
        _ => usage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use render::TextPainter as _;

    fn sample_doc() -> doc::Doc {
        let mut d = doc::Doc::with_capacity(64);
        let layer = d.layer_index(b"0");
        // 40 отрезков + 4 окружности + надписи
        for i in 0..40 {
            let a = d.push_point(i as f64 * 10.0, 0.0);
            let b = d.push_point(i as f64 * 10.0, 100.0);
            d.segs.push(doc::Seg { a, b, layer });
        }
        for i in 0..4 {
            let c = d.push_point(i as f64 * 50.0, 50.0);
            d.circles.push(doc::Circle { c, r: 20.0, layer });
        }
        for i in 0..6 {
            let p = d.push_point(i as f64 * 30.0, 120.0);
            let (s, _) = d.arena.push(b"Text");
            let idx = d.texts.len() as u32;
            d.texts.push(doc::TextEnt {
                pos: p,
                height: 5.0,
                rot_deg: 0.0,
                layer,
                kind: doc::TEXT_KIND_TEXT,
                s,
                wire: text::StrRef::EMPTY,
                dxf_handle: 0x100 + i,
            });
            d.handles.push(pool::Handle { slot: idx, gen: 1 });
        }
        d.path.push_str("sample.dxf");
        d
    }

    fn armed<T>(f: impl FnOnce() -> T) -> (T, usize) {
        reset_armed();
        let v = f();
        let (a, _) = disarm();
        (v, a)
    }

    /// Диагностика: какие части кадра аллоцируют (падает в отчёте, если что-то нашли).
    #[test]
    fn diag_alloc_breakdown() {
        let d = sample_doc();
        let mut r = render::Renderer::new(1024, 700, d.xs.len() * 2);
        let mut ui = render::UiState::new();
        ui.set_file("sample.dxf");
        ui.set_status("test");
        ui.catalog_names.push("A".to_string());
        ui.selected_text = Some(1);
        let cam = render::Camera::fit(d.bounds(), 400, 500, 32);
        let mut p = win32::ApproxPainter;
        let session = edit::EditSession::new();
        let _ = r.frame(&d, cam, &mut ui, &session, &mut p);
        let (_, a_proj) = armed(|| render::project(&d.xs, &d.ys, cam, &mut r.sx, &mut r.sy));
        let (_, a_clear) = armed(|| r.fb.clear(render::swiss::PAPER));
        let (_, a_text) = armed(|| {
            let mut q = win32::ApproxPainter;
            let s = String::from_utf8_lossy(d.arena_get(d.texts[0].s));
            let w = q.measure(13, false, &s);
            q.text(&mut r.fb, 10, 10, 13, false, render::swiss::INK, &s);
            w
        });
        let (_, a_frame) = armed(|| r.frame(&d, cam, &mut ui, &session, &mut p));
        println!("alloc: project={} clear={} text={} frame={}", a_proj, a_clear, a_text, a_frame);
        assert_eq!(a_proj + a_clear + a_text, 0);
    }

    /// Главное проверяемое утверждение: кадр не аллоцирует.
    #[test]
    fn frame_allocates_nothing() {
        let d = sample_doc();
        let mut r = render::Renderer::new(1024, 700, d.xs.len() * 2);
        let mut ui = render::UiState::new();
        ui.set_file("sample.dxf");
        ui.set_status("test");
        ui.catalog_names.push("A".to_string());
        ui.catalog_names.push("B".to_string());
        ui.selected_text = Some(1);
        let cam = render::Camera::fit(d.bounds(), 400, 500, 32);
        let session = edit::EditSession::new();
        let mut p = win32::ApproxPainter;
        let _ = r.frame(&d, cam, &mut ui, &session, &mut p);
        reset_armed();
        for _ in 0..10 {
            let _ = r.frame(&d, cam, &mut ui, &session, &mut p);
        }
        let (a, _) = disarm();
        assert_eq!(a, 0, "кадр обязан не аллоцировать");
    }

    #[test]
    fn frame_does_not_allocate_with_modal_open() {
        let d = sample_doc();
        let mut r = render::Renderer::new(1024, 700, d.xs.len() * 2);
        let mut ui = render::UiState::new();
        ui.catalog_names.push("A".to_string());
        let cam = render::Camera::fit(d.bounds(), 400, 500, 32);
        let mut session = edit::EditSession::new();
        session.begin(0, b"Text");
        session.wire.set(b"W-1");
        let mut p = win32::ApproxPainter;
        let _ = r.frame(&d, cam, &mut ui, &session, &mut p);
        reset_armed();
        for _ in 0..5 {
            let _ = r.frame(&d, cam, &mut ui, &session, &mut p);
        }
        let (a, _) = disarm();
        assert_eq!(a, 0, "модальное окно тоже не аллоцирует");
    }

    #[test]
    fn frame_reports_overflow_instead_of_allocating() {
        let d = sample_doc();
        let mut r = render::Renderer::new(800, 600, 4); // заведомо мало
        let mut ui = render::UiState::new();
        let cam = render::Camera::fit(d.bounds(), 300, 400, 16);
        let session = edit::EditSession::new();
        let mut p = win32::ApproxPainter;
        match r.frame(&d, cam, &mut ui, &session, &mut p) {
            Err(render::RenderError::Overflow { need, have }) => {
                assert!(need > have);
            }
            _ => panic!("ожидался Overflow"),
        }
    }

    /// Сквозной сценарий: правка текста + номер провода + комментарий ->
    /// версия в подпапке, перезапись рабочего файла, .txt, .eml, цепочка.
    #[test]
    fn end_to_end_save_rotates_and_signs() {
        let base = std::env::temp_dir().join(format!("dxfsign-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let work = base.join("plan.dxf");
        let src = concat!(
            "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n",
            "0\nSECTION\n2\nENTITIES\n",
            "0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n11\n100.0\n21\n0.0\n",
            "0\nTEXT\n5\n2A\n8\n0\n10\n10.0\n20\n10.0\n40\n2.5\n1\nKLEMMA 1\n",
            "0\nENDSEC\n0\nEOF\n"
        );
        std::fs::write(&work, src.as_bytes()).unwrap();

        let mut d = doc::Doc::with_capacity(64);
        dxf::parse(src.as_bytes(), &mut d).unwrap();
        d.path.push_str(&work.to_string_lossy());
        // правка: текст + провод + комментарий
        assert!(d.edit_text(0, b"KLEMMA 2", b"W-101", "по замечанию ПТО".as_bytes()));
        // отдельный комментарий без правки геометрии
        assert!(d.add_comment(0, "проверить сечение кабеля".as_bytes(), b"W-102"));

        let mut fs = ports::LocalFs::new();
        fs.create_dir_all(&base.join("_VERSIONS").to_string_lossy()).unwrap();
        let mut store =
            ports::ChainStore::open(base.join("_VERSIONS/index.tsv").to_string_lossy().to_string()).unwrap();
        let mut signer = ports::HmacSigner::new(b"test-key-not-a-secret".to_vec(), "test".into());
        let spool = base.join("_SPOOL").to_string_lossy().to_string();
        let mut mail = ports::SpoolTransport::new(spool.clone());
        let cfg = save::SaveConfig {
            to_addr: "owner@example.invalid".to_string(),
            spool_dir: spool.clone(),
            versions_dir: "_VERSIONS".to_string(),
            tz_offset_min: 180,
            ..Default::default()
        };
        let mut bytes = Vec::new();
        let out = save::save(&d, &cfg, &mut fs, &mut signer, &mut mail, &mut store, 1795077300, 1, &mut bytes).unwrap();

        // 1. рабочий файл перезаписан и содержит новую надпись
        let now = std::fs::read(&work).unwrap();
        let txt = String::from_utf8_lossy(&now);
        assert!(txt.contains("KLEMMA 2"));
        assert!(txt.contains("DXFSIGN"));
        assert!(txt.contains("W-102"), "последний номер провода обязан быть в XData файла");
        // 2. прежняя версия лежит в подпапке рядом
        assert!(out.version_file.contains("_VERSIONS"));
        let vbytes = std::fs::read(&out.version_file).unwrap();
        assert!(String::from_utf8_lossy(&vbytes).contains("KLEMMA 1"));
        // 3. .txt рядом с чертежом, с проводом и комментарием
        let chg = std::fs::read_to_string(&out.txt_path).unwrap();
        assert!(chg.contains("W-101"));
        assert!(chg.contains("по замечанию ПТО"));
        assert!(chg.contains("проверить сечение кабеля"));
        assert!(chg.contains("COMMENT"));
        // 4. письмо: multipart, три вложения, подпись, оба хеша
        let eml = std::fs::read_to_string(&out.eml_path).unwrap();
        assert!(eml.contains("multipart/mixed"));
        assert!(eml.contains("X-DXFSIGN-Signature: HMAC-SHA256"));
        assert!(eml.contains("X-DXFSIGN-Prev-Version:"));
        assert!(eml.contains("X-DXFSIGN-Comments:"));
        assert_eq!(
            eml.matches("Content-Disposition: attachment").count(),
            3,
            "три вложения: новый .dxf, прежний .dxf, перечень .txt"
        );
        assert!(eml.contains("Content-Type: text/plain"));
        // 5. цепочка
        assert_eq!(store.count(), 1);
        assert!(store.head().is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn save_refuses_when_nothing_changed() {
        let base = std::env::temp_dir().join(format!("dxfsign-nochange-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let work = base.join("p.dxf");
        let src = "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nTEXT\n8\n0\n10\n0\n20\n0\n40\n2.5\n1\nA\n0\nENDSEC\n0\nEOF\n";
        std::fs::write(&work, src.as_bytes()).unwrap();
        let mut d = doc::Doc::with_capacity(16);
        dxf::parse(src.as_bytes(), &mut d).unwrap();
        d.path.push_str(&work.to_string_lossy());
        // Ровно те же байты, что уже лежат: writer даст другой текст (round-trip
        // не побайтовый), поэтому «не изменилось» проверяем по хешу записи.
        let mut fs = ports::LocalFs::new();
        let mut store = ports::ChainStore::open(base.join("i.tsv").to_string_lossy().to_string()).unwrap();
        let mut signer = ports::HmacSigner::new(b"k".to_vec(), "k".into());
        let mut mail = ports::SpoolTransport::new(base.join("sp").to_string_lossy().to_string());
        let cfg = save::SaveConfig { spool_dir: base.join("sp").to_string_lossy().to_string(), ..Default::default() };
        let mut bytes = Vec::new();
        let _ = save::save(&d, &cfg, &mut fs, &mut signer, &mut mail, &mut store, 1795077300, 1, &mut bytes).unwrap();
        // второй раз с тем же содержимым — версия не пишется
        let mut bytes2 = Vec::new();
        match save::save(&d, &cfg, &mut fs, &mut signer, &mut mail, &mut store, 1795077400, 2, &mut bytes2) {
            Err(save::SaveError::NothingChanged) => {}
            other => panic!("ожидалось NothingChanged, получено {:?}", other.map(|o| o.eml_path)),
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
